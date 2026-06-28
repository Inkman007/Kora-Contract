#![no_std]

use kora_shared::{
    errors::KoraError,
    events,
    reentrancy::ReentrancyGuard,
    validation::{require_valid_fee_bps, UPGRADE_TIMELOCK_DELAY},
};
use soroban_sdk::{contract, contractimpl, contracttype, token, Address, BytesN, Env};

// ── Storage TTL constants (~31 days in ledgers) ───────────────────────────────
const PERSISTENT_BUMP_AMOUNT: u32 = 535_680;
const PERSISTENT_LIFETIME_THRESHOLD: u32 = 535_680 / 2;

// ── Rate-limit epoch: 24 h in seconds ────────────────────────────────────────
const EPOCH_DURATION: u64 = 86_400;

// ── Storage Keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    /// Admin address — persistent so it survives ledger archival.
    Admin,
    /// Protocol fee in basis points — persistent for durability.
    FeeBps,
    /// Accumulated fees per token (informational).
    Collected(Address),
    /// Whitelisted token flag.
    WhitelistedToken(Address),
    /// Pending upgrade proposal: (wasm_hash, proposed_at_timestamp).
    UpgradeProposal,
    /// Reentrancy guard flag (instance-level).
    WithdrawalLock,
    /// Maximum total withdrawal allowed per EPOCH_DURATION window (0 = uncapped).
    WithdrawalCap,
    /// Pending cap change proposal: (new_cap, proposed_at).
    WithdrawalCapProposal,
    /// Timestamp when the current rate-limit epoch started.
    EpochStart,
    /// Total withdrawn in the current epoch (resets each epoch).
    EpochWithdrawn,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct TreasuryContract;

#[contractimpl]
impl TreasuryContract {
    /// One-time initialization. Sets admin and protocol fee.
    pub fn initialize(env: Env, admin: Address, fee_bps: u32) -> Result<(), KoraError> {
        if env.storage().persistent().has(&DataKey::Admin) {
            return Err(KoraError::AlreadyInitialized);
        }
        require_valid_fee_bps(fee_bps)?;
        kora_shared::validation::require_not_self(&env, &admin)?;
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage().instance().set(&DataKey::WithdrawalLock, &false);
        env.storage().persistent().set(&DataKey::Admin, &admin);
        env.storage().persistent().extend_ttl(
            &DataKey::Admin,
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
        env.storage().persistent().set(&DataKey::FeeBps, &fee_bps);
        env.storage().persistent().extend_ttl(
            &DataKey::FeeBps,
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
        // Initialize rate-limit state: uncapped, epoch starts now, zero withdrawn.
        env.storage().instance().set(&DataKey::WithdrawalCap, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::EpochStart, &env.ledger().timestamp());
        env.storage().instance().set(&DataKey::EpochWithdrawn, &0i128);
        events::treasury_initialized(&env, &admin, fee_bps);
        Ok(())
    }

    /// Update protocol fee. Admin only.
    pub fn set_fee_bps(env: Env, admin: Address, fee_bps: u32) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        require_valid_fee_bps(fee_bps)?;

        let old_bps: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::FeeBps)
            .unwrap_or(50);

        env.storage().persistent().set(&DataKey::FeeBps, &fee_bps);
        Self::bump_persistent(&env, &DataKey::FeeBps);

        events::fee_rate_updated(&env, &admin, old_bps, fee_bps);
        Ok(())
    }

    /// Whitelist a token so it can be used in withdraw / emergency_withdraw.
    /// Admin only.
    pub fn whitelist_token(env: Env, admin: Address, token: Address) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;

        env.storage()
            .persistent()
            .set(&DataKey::WhitelistedToken(token.clone()), &true);
        Self::bump_persistent(&env, &DataKey::WhitelistedToken(token.clone()));

        events::token_whitelisted(&env, &token);
        Ok(())
    }

    /// Record an incoming fee for a given token. Called by the marketplace after
    /// transferring the fee amount to this contract. Updates the informational
    /// accounting ledger.
    ///
    /// No auth required — the token transfer itself is the proof of payment.
    /// The amount is validated to be > 0 to prevent no-op accounting entries.
    pub fn collect_fee(env: Env, token: Address, amount: i128) -> Result<(), KoraError> {
        if amount <= 0 {
            return Err(KoraError::InvalidAmount);
        }
        Self::require_whitelisted_token(&env, &token)?;

        let key = DataKey::Collected(token.clone());
        let current: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        let new_total = current
            .checked_add(amount)
            .ok_or(KoraError::ArithmeticOverflow)?;

        env.storage().persistent().set(&key, &new_total);
        Self::bump_persistent(&env, &key);

        events::fee_collected(&env, 0, amount, &token);
        Ok(())
    }

    /// Withdraw accumulated fees to a recipient. Admin only.
    /// Subject to the rolling 24 h withdrawal cap.
    /// Protected against reentrancy via RAII ReentrancyGuard.
    pub fn withdraw(
        env: Env,
        admin: Address,
        token: Address,
        recipient: Address,
        amount: i128,
    ) -> Result<(), KoraError> {
        // ── Checks ────────────────────────────────────────────────────────────
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        if amount <= 0 {
            return Err(KoraError::InvalidAmount);
        }
        Self::require_whitelisted_token(&env, &token)?;
        Self::enforce_rate_limit(&env, amount)?;

        // Acquire reentrancy guard — released automatically when _guard drops
        let _guard = ReentrancyGuard::new(&env)?;

        let token_client = token::Client::new(&env, &token);
        let balance = token_client.balance(&env.current_contract_address());

        if balance < amount {
            return Err(KoraError::InsufficientPoolBalance);
        }

        // ── Effects ───────────────────────────────────────────────────────────
        let collected_key = DataKey::Collected(token.clone());
        if let Some(collected) = env
            .storage()
            .persistent()
            .get::<_, i128>(&collected_key)
        {
            let new_collected = collected.saturating_sub(amount);
            env.storage().persistent().set(&collected_key, &new_collected);
            Self::bump_persistent(&env, &collected_key);
        }
        Self::record_withdrawal(&env, amount);

        // ── Interactions ──────────────────────────────────────────────────────
        token_client.transfer(&env.current_contract_address(), &recipient, &amount);

        events::fee_withdrawn(&env, &token, amount);
        Ok(())
    }

    /// Emergency drain — withdraw entire token balance. Admin only.
    /// Subject to the rolling 24 h withdrawal cap.
    /// Protected against reentrancy via RAII ReentrancyGuard.
    pub fn emergency_withdraw(
        env: Env,
        admin: Address,
        token: Address,
        recipient: Address,
    ) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        Self::require_whitelisted_token(&env, &token)?;

        let _guard = ReentrancyGuard::new(&env)?;

        let token_client = token::Client::new(&env, &token);
        let balance = token_client.balance(&env.current_contract_address());

        if balance > 0 {
            Self::enforce_rate_limit(&env, balance)?;
            Self::record_withdrawal(&env, balance);
            token_client.transfer(&env.current_contract_address(), &recipient, &balance);
            events::emergency_withdrawn(&env, &admin, &token, balance);
        }

        Ok(())
    }

    // ── Withdrawal cap management ─────────────────────────────────────────────

    /// Propose a new withdrawal cap. Takes effect after UPGRADE_TIMELOCK_DELAY.
    /// Admin only. Set cap to 0 to remove the limit entirely.
    pub fn propose_withdrawal_cap(
        env: Env,
        admin: Address,
        new_cap: i128,
    ) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        if new_cap < 0 {
            return Err(KoraError::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::WithdrawalCapProposal, &(new_cap, env.ledger().timestamp()));
        events::withdrawal_cap_proposed(&env, &admin, new_cap);
        Ok(())
    }

    /// Execute a previously proposed withdrawal cap change after the timelock elapses.
    /// Admin only.
    pub fn execute_withdrawal_cap(env: Env, admin: Address) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        let (new_cap, proposed_at): (i128, u64) = env
            .storage()
            .instance()
            .get(&DataKey::WithdrawalCapProposal)
            .ok_or(KoraError::NoCapChangeProposed)?;
        if env.ledger().timestamp() < proposed_at + UPGRADE_TIMELOCK_DELAY {
            return Err(KoraError::WithdrawalCapTimelockNotElapsed);
        }
        let old_cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::WithdrawalCap)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::WithdrawalCap, &new_cap);
        env.storage()
            .instance()
            .remove(&DataKey::WithdrawalCapProposal);
        events::withdrawal_cap_updated(&env, &admin, old_cap, new_cap);
        Ok(())
    }

    /// Returns the current withdrawal cap (0 = uncapped).
    pub fn get_withdrawal_cap(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::WithdrawalCap)
            .unwrap_or(0)
    }

    /// Returns the current protocol fee in basis points.
    pub fn get_fee_bps(env: Env) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::FeeBps)
            .unwrap_or(50)
    }

    /// Returns the live token balance held by this contract.
    pub fn get_balance(env: Env, token: Address) -> i128 {
        token::Client::new(&env, &token).balance(&env.current_contract_address())
    }

    pub fn get_admin(env: Env) -> Result<Address, KoraError> {
        env.storage()
            .persistent()
            .get(&DataKey::Admin)
            .ok_or(KoraError::NotInitialized)
    }

    // ── Upgrade ────────────────────────────────────────────────────────────────

    pub fn propose_upgrade(
        env: Env,
        admin: Address,
        new_wasm_hash: BytesN<32>,
    ) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        env.storage()
            .instance()
            .set(&DataKey::UpgradeProposal, &(new_wasm_hash.clone(), env.ledger().timestamp()));
        events::upgrade_proposed(&env, &admin, &new_wasm_hash);
        Ok(())
    }

    pub fn execute_upgrade(env: Env, admin: Address) -> Result<(), KoraError> {
        admin.require_auth();
        Self::require_admin(&env, &admin)?;
        let (wasm_hash, proposed_at): (BytesN<32>, u64) = env
            .storage()
            .instance()
            .get(&DataKey::UpgradeProposal)
            .ok_or(KoraError::NoUpgradeProposed)?;
        if env.ledger().timestamp() < proposed_at + UPGRADE_TIMELOCK_DELAY {
            return Err(KoraError::UpgradeTimelockNotElapsed);
        }
        env.storage().instance().remove(&DataKey::UpgradeProposal);
        events::upgrade_executed(&env, &admin, &wasm_hash);
        env.deployer().update_current_contract_wasm(wasm_hash);
        Ok(())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn require_admin(env: &Env, caller: &Address) -> Result<(), KoraError> {
        let admin: Address = env
            .storage()
            .persistent()
            .get(&DataKey::Admin)
            .ok_or(KoraError::NotInitialized)?;
        if &admin != caller {
            return Err(KoraError::NotAdmin);
        }
        Ok(())
    }

    fn require_whitelisted_token(env: &Env, token: &Address) -> Result<(), KoraError> {
        let whitelisted: bool = env
            .storage()
            .persistent()
            .get(&DataKey::WhitelistedToken(token.clone()))
            .unwrap_or(false);
        if !whitelisted {
            return Err(KoraError::TokenNotWhitelisted);
        }
        Ok(())
    }

    /// Advance the epoch if 24 h have elapsed, then check the cap.
    fn enforce_rate_limit(env: &Env, amount: i128) -> Result<(), KoraError> {
        let cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::WithdrawalCap)
            .unwrap_or(0);
        if cap == 0 {
            return Ok(());
        }

        let now = env.ledger().timestamp();
        let epoch_start: u64 = env
            .storage()
            .instance()
            .get(&DataKey::EpochStart)
            .unwrap_or(now);

        let epoch_withdrawn: i128 = if now.saturating_sub(epoch_start) >= EPOCH_DURATION {
            // New epoch: reset counters.
            env.storage().instance().set(&DataKey::EpochStart, &now);
            env.storage().instance().set(&DataKey::EpochWithdrawn, &0i128);
            0
        } else {
            env.storage()
                .instance()
                .get(&DataKey::EpochWithdrawn)
                .unwrap_or(0)
        };

        let new_total = epoch_withdrawn
            .checked_add(amount)
            .ok_or(KoraError::ArithmeticOverflow)?;
        if new_total > cap {
            return Err(KoraError::WithdrawalRateLimitExceeded);
        }
        Ok(())
    }

    /// Record a successful withdrawal against the current epoch's running total.
    fn record_withdrawal(env: &Env, amount: i128) {
        let cap: i128 = env
            .storage()
            .instance()
            .get(&DataKey::WithdrawalCap)
            .unwrap_or(0);
        if cap == 0 {
            return;
        }
        let current: i128 = env
            .storage()
            .instance()
            .get(&DataKey::EpochWithdrawn)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::EpochWithdrawn, &current.saturating_add(amount));
    }

    fn bump_persistent(env: &Env, key: &DataKey) {
        env.storage().persistent().extend_ttl(
            key,
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    fn setup() -> (Env, Address, TreasuryContractClient<'static>) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TreasuryContract);
        let client = TreasuryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin, &50u32);
        (env, admin, client)
    }

    #[test]
    fn test_initialize_creates_contract() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TreasuryContract);
        let client = TreasuryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        assert!(client.try_initialize(&admin, &50u32).is_ok());
        assert_eq!(client.get_fee_bps(), 50);
    }

    #[test]
    fn test_initialize_already_initialized() {
        let (env, admin, client) = setup();
        let result = client.try_initialize(&admin, &50u32);
        assert!(result.is_err());
    }

    #[test]
    fn test_initialize_invalid_fee_bps() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TreasuryContract);
        let client = TreasuryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        assert!(client.try_initialize(&admin, &10_001u32).is_err());
    }

    #[test]
    fn test_get_fee_bps_after_init() {
        let (_env, _admin, client) = setup();
        assert_eq!(client.get_fee_bps(), 50);
    }

    // ── set_fee_bps ───────────────────────────────────────────────────────────

    #[test]
    fn test_set_fee_bps_success() {
        let (_env, admin, client) = setup();
        client.set_fee_bps(&admin, &100u32);
        assert_eq!(client.get_fee_bps(), 100);
    }

    #[test]
    fn test_set_fee_bps_requires_admin() {
        let (env, _admin, client) = setup();
        let non_admin = Address::generate(&env);
        assert!(client.try_set_fee_bps(&non_admin, &100u32).is_err());
    }

    #[test]
    fn test_set_fee_bps_invalid_bps_fails() {
        let (_env, admin, client) = setup();
        assert!(client.try_set_fee_bps(&admin, &10_001u32).is_err());
    }

    #[test]
    fn test_set_fee_bps_zero_allowed() {
        let (_env, admin, client) = setup();
        client.set_fee_bps(&admin, &0u32);
        assert_eq!(client.get_fee_bps(), 0);
    }

    #[test]
    fn test_set_fee_bps_max_allowed() {
        let (_env, admin, client) = setup();
        client.set_fee_bps(&admin, &10_000u32);
        assert_eq!(client.get_fee_bps(), 10_000);
    }

    #[test]
    fn test_set_fee_bps_over_max_fails() {
        let (_env, admin, client) = setup();
        assert!(client.try_set_fee_bps(&admin, &10_001u32).is_err());
    }

    #[test]
    fn test_set_fee_bps_multiple_updates() {
        let (_env, admin, client) = setup();
        client.set_fee_bps(&admin, &100u32);
        assert_eq!(client.get_fee_bps(), 100);
        client.set_fee_bps(&admin, &200u32);
        assert_eq!(client.get_fee_bps(), 200);
        client.set_fee_bps(&admin, &50u32);
        assert_eq!(client.get_fee_bps(), 50);
    }

    // ── withdraw ──────────────────────────────────────────────────────────────

    #[test]
    fn test_withdraw_requires_admin() {
        let (env, _admin, client) = setup();
        let non_admin = Address::generate(&env);
        let token = Address::generate(&env);
        let recipient = Address::generate(&env);
        assert!(client
            .try_withdraw(&non_admin, &token, &recipient, &1_000_000i128)
            .is_err());
    }

    #[test]
    fn test_withdraw_zero_amount_fails() {
        let (env, admin, client) = setup();
        let token = Address::generate(&env);
        let recipient = Address::generate(&env);
        assert!(client
            .try_withdraw(&admin, &token, &recipient, &0i128)
            .is_err());
    }

    #[test]
    fn test_withdraw_with_negative_amount_rejected() {
        let (env, admin, client) = setup();
        let token = Address::generate(&env);
        let recipient = Address::generate(&env);
        assert!(client
            .try_withdraw(&admin, &token, &recipient, &-1_000i128)
            .is_err());
    }

    #[test]
    fn test_emergency_withdraw_requires_admin() {
        let (env, _admin, client) = setup();
        let non_admin = Address::generate(&env);
        let token = Address::generate(&env);
        let recipient = Address::generate(&env);
        assert!(client
            .try_emergency_withdraw(&non_admin, &token, &recipient)
            .is_err());
    }

    #[test]
    fn test_get_fee_bps_default_before_init() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TreasuryContract);
        let client = TreasuryContractClient::new(&env, &contract_id);
        assert_eq!(client.get_fee_bps(), 50);
    }

    #[test]
    fn test_lock_released_after_failed_withdraw() {
        let (env, admin, client) = setup();
        let token = Address::generate(&env);
        let recipient = Address::generate(&env);
        // Fails due to token not whitelisted — lock must be released
        let _ = client.try_withdraw(&admin, &token, &recipient, &1_000i128);
        // Subsequent admin operation must succeed (lock not stuck)
        assert!(client.try_set_fee_bps(&admin, &100u32).is_ok());
    }

    #[test]
    fn test_lock_released_after_emergency_withdraw() {
        let (env, admin, client) = setup();
        let token = Address::generate(&env);
        let recipient = Address::generate(&env);
        let _ = client.try_emergency_withdraw(&admin, &token, &recipient);
        // Lock must be released regardless of balance
        assert!(client.try_set_fee_bps(&admin, &100u32).is_ok());
    }

    #[test]
    fn test_initialize_self_as_admin_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, TreasuryContract);
        let client = TreasuryContractClient::new(&env, &contract_id);
        let result = client.try_initialize(&contract_id, &50u32);
        assert!(result.is_err());
    }

    // ── withdrawal rate limit ─────────────────────────────────────────────────

    #[test]
    fn test_withdrawal_cap_default_is_uncapped() {
        let (_env, _admin, client) = setup();
        assert_eq!(client.get_withdrawal_cap(), 0);
    }

    #[test]
    fn test_propose_withdrawal_cap_requires_admin() {
        let (env, _admin, client) = setup();
        let non_admin = Address::generate(&env);
        assert!(client.try_propose_withdrawal_cap(&non_admin, &1_000_000i128).is_err());
    }

    #[test]
    fn test_propose_negative_cap_rejected() {
        let (_env, admin, client) = setup();
        assert!(client.try_propose_withdrawal_cap(&admin, &-1i128).is_err());
    }

    #[test]
    fn test_execute_cap_before_timelock_fails() {
        let (_env, admin, client) = setup();
        client.propose_withdrawal_cap(&admin, &1_000_000i128);
        // Timelock hasn't elapsed
        assert!(client.try_execute_withdrawal_cap(&admin).is_err());
    }

    #[test]
    fn test_execute_cap_without_proposal_fails() {
        let (_env, admin, client) = setup();
        assert!(client.try_execute_withdrawal_cap(&admin).is_err());
    }
}
