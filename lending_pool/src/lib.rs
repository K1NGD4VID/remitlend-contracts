#![no_std]
use soroban_sdk::token::Client as TokenClient;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, Symbol,
};

mod events;
use events::*;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PoolError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ContractPaused = 3,
    InvalidAmount = 4,
    PoolSizeExceeded = 5,
    InsufficientBalance = 6,
    InsufficientLiquidity = 7,
    InvalidMaxPoolSize = 9,
    NoProposedAdmin = 10,
    CooldownTooLong = 11,
    NotPaused = 12,
    WithdrawalCooldownActive = 13,
}

/// Storage keys.
///
/// v2 replaces the accumulator-style keys (Deposit, RewardDebt, ClaimableYield,
/// AccYieldPerDeposit, UnclaimedYieldPool) with a share-based (LP-token) model.
/// Yield is implicit in the exchange rate between shares and underlying assets —
/// no separate accumulation or claim step is required.
///
/// ## Accounting source of truth (v3, issue #2)
///
/// Share value is derived **exclusively** from `TotalManagedAssets`, an
/// internally-tracked figure equal to deposited principal plus realized yield,
/// net of withdrawals. It is deliberately *not* derived from the contract's raw
/// `TokenClient::balance`, because the raw balance:
///   * drops while principal is out on loan (the principal is still a pool asset,
///     just a receivable — share value must not swing with utilisation), and
///   * can be inflated by anyone transferring tokens directly to the pool
///     address (a donation / inflation attack that would otherwise let an
///     attacker arbitrarily move existing holders' redeemable value).
///
/// The raw balance is used only as a *liquidity* gate: a redemption that cannot
/// currently be serviced from on-hand tokens fails with `InsufficientLiquidity`
/// rather than mis-pricing shares. Realized yield (e.g. loan interest repaid
/// into the pool) is folded into `TotalManagedAssets` exclusively through the
/// admin-gated `record_yield` entry point, so unsolicited transfers never change
/// the exchange rate.
///
/// All per-token keys carry the token address so one contract instance can
/// serve multiple token liquidity pools.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Paused,
    WithdrawalCooldown,
    /// token → max pool size cap (0 = unlimited)
    MaxPoolSize(Address),
    /// token → total LP shares outstanding across all providers
    TotalShares(Address),
    /// (provider, token) → LP shares held
    Shares(Address, Address),
    /// (provider, token) → ledger sequence of the most recent deposit
    DepositTimestamp(Address, Address),
    /// token → total principal deposited (net of withdrawals); used for
    /// utilisation stats and the MaxPoolSize cap. Tracks principal only — it is
    /// never moved by yield, so it cannot drift above what was actually
    /// deposited.
    TotalDeposits(Address),
    /// token → total underlying assets backing LP shares (principal + realized
    /// yield, net of withdrawals). Single source of truth for the share↔asset
    /// exchange rate. Unaffected by direct token transfers or by principal
    /// temporarily out on loan. See the module-level accounting note.
    TotalManagedAssets(Address),
    /// token → address authorized to report realized yield via `record_yield`
    /// (normally the LoanManager that repayments flow through). Lets the
    /// repayment path credit interest to LPs automatically while keeping
    /// `record_yield` gated, so arbitrary callers still cannot move the share
    /// price.
    LoanManager(Address),
    /// token → number of active depositors
    DepositorCount(Address),
    ProposedAdmin,
    Version,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolStats {
    pub total_deposits: i128,
    pub total_shares: i128,
    pub pool_token_balance: i128,
    pub depositor_count: u32,
    /// `(total_deposits − pool_token_balance) / total_deposits × 10 000`.
    /// Positive only when outstanding loans have reduced pool_balance below
    /// tracked principal.  Note: accrued yield increases pool_balance, which
    /// partially offsets outstanding loans in this formula — utilisation may
    /// therefore understate the true loan fraction when significant yield has
    /// accumulated relative to outstanding principal.
    pub utilization_bps: u32,
}

#[contract]
pub struct LendingPool;

#[contractimpl]
impl LendingPool {
    const INSTANCE_TTL_THRESHOLD: u32 = 17280;
    const INSTANCE_TTL_BUMP: u32 = 518400;
    const PERSISTENT_TTL_THRESHOLD: u32 = 17280;
    const PERSISTENT_TTL_BUMP: u32 = 518400;
    const CURRENT_VERSION: u32 = 4;
    const DEFAULT_WITHDRAWAL_COOLDOWN: u32 = 1_440;
    const SHARE_PRICE_SCALE: i128 = 1_000_000;
    const MAX_WITHDRAWAL_COOLDOWN_LEDGERS: u32 = 17_280 * 30;

    // ── TTL helpers ───────────────────────────────────────────────────────

    fn bump_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(Self::INSTANCE_TTL_THRESHOLD, Self::INSTANCE_TTL_BUMP);
    }

    fn bump_persistent_ttl(env: &Env, key: &DataKey) {
        env.storage().persistent().extend_ttl(
            key,
            Self::PERSISTENT_TTL_THRESHOLD,
            Self::PERSISTENT_TTL_BUMP,
        );
    }

    // ── Storage accessors ─────────────────────────────────────────────────

    fn admin(env: &Env) -> Address {
        Self::bump_instance_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized")
    }

    fn read_pool_balance(env: &Env, token: &Address) -> i128 {
        TokenClient::new(env, token).balance(&env.current_contract_address())
    }

    fn total_deposits(env: &Env, token: &Address) -> i128 {
        Self::bump_instance_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::TotalDeposits(token.clone()))
            .unwrap_or(0)
    }

    fn total_shares(env: &Env, token: &Address) -> i128 {
        Self::bump_instance_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::TotalShares(token.clone()))
            .unwrap_or(0)
    }

    /// Total underlying assets backing LP shares (principal + realized yield,
    /// net of withdrawals). The single source of truth for the share↔asset
    /// exchange rate — see the module-level accounting note.
    fn total_managed_assets(env: &Env, token: &Address) -> i128 {
        Self::bump_instance_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::TotalManagedAssets(token.clone()))
            .unwrap_or(0)
    }

    fn set_total_managed_assets(env: &Env, token: &Address, value: i128) {
        env.storage()
            .instance()
            .set(&DataKey::TotalManagedAssets(token.clone()), &value);
        Self::bump_instance_ttl(env);
    }

    /// Address authorized to report realized yield for `token`, if configured.
    fn loan_manager(env: &Env, token: &Address) -> Option<Address> {
        Self::bump_instance_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::LoanManager(token.clone()))
    }

    fn read_shares(env: &Env, provider: &Address, token: &Address) -> i128 {
        let key = DataKey::Shares(provider.clone(), token.clone());
        let shares: i128 = env.storage().persistent().get(&key).unwrap_or(0);
        if shares > 0 {
            Self::bump_persistent_ttl(env, &key);
        }
        shares
    }

    fn read_deposit_timestamp(env: &Env, provider: &Address, token: &Address) -> Option<u32> {
        let key = DataKey::DepositTimestamp(provider.clone(), token.clone());
        let deposit_ledger: Option<u32> = env.storage().persistent().get(&key);
        if deposit_ledger.is_some() {
            Self::bump_persistent_ttl(env, &key);
        }
        deposit_ledger
    }

    fn read_depositor_count(env: &Env, token: &Address) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::DepositorCount(token.clone()))
            .unwrap_or(0)
    }

    fn withdrawal_cooldown(env: &Env) -> u32 {
        Self::bump_instance_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::WithdrawalCooldown)
            .unwrap_or(Self::DEFAULT_WITHDRAWAL_COOLDOWN)
    }

    fn assert_not_paused(env: &Env) -> Result<(), PoolError> {
        Self::bump_instance_ttl(env);
        let paused: bool = env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false);
        if paused {
            return Err(PoolError::ContractPaused);
        }
        Ok(())
    }

    // ── Share / asset math ────────────────────────────────────────────────

    /// LP shares to mint for `amount` of deposited assets.
    ///
    /// Minimum amount the very first depositor of a token pool must supply.
    /// Combined with the explicit `shares_to_mint <= 0` revert this raises
    /// the cost of the donation/inflation attack (issue #1) — an attacker
    /// can no longer mint a single share for a tiny amount and then donate
    /// tokens to make a later depositor's mint round to zero, because the
    /// first deposit itself must be large enough to make any donation
    /// economically irrational. Subsequent deposits are unaffected, so the
    /// exact 1:1 exchange-rate math the rest of the contract relies on is
    /// preserved.
    const MINIMUM_INITIAL_DEPOSIT: i128 = 1_000;

    /// The first depositor always receives a 1-for-1 allocation.  Subsequent
    /// depositors receive `amount * total_shares / total_assets_before` so
    /// that the exchange rate is preserved and existing holders are not
    /// diluted.
    fn calc_shares_to_mint(
        amount: i128,
        total_assets_before: i128,
        cur_total_shares: i128,
    ) -> i128 {
        if cur_total_shares == 0 {
            // First depositor into an empty share pool: 1:1 allocation.
            amount
        } else {
            // Invariant: once shares exist, managed assets are strictly positive.
            // deposit and redeem move shares and managed assets together, and
            // record_yield only ever increases managed assets (and requires
            // shares > 0), so `total_assets_before == 0` here is unreachable. The
            // checked_div below would surface any violation as a panic rather
            // than silently minting a diluting 1:1 allocation.
            debug_assert!(
                total_assets_before > 0,
                "managed assets must be positive while shares are outstanding"
            );
            amount
                .checked_mul(cur_total_shares)
                .and_then(|v| v.checked_div(total_assets_before))
                .expect("share mint overflow")
        }
    }

    /// Underlying assets redeemable for `shares` given current pool state.
    ///
    /// Returns `shares * total_assets / total_shares`, which automatically
    /// includes any yield that has accumulated since the shares were minted.
    /// Returns a typed `PoolError` instead of panicking on the zero-divisor
    /// pathologies that would otherwise trap funds.
    fn calc_assets_to_redeem(
        shares: i128,
        total_assets: i128,
        cur_total_shares: i128,
    ) -> Result<i128, PoolError> {
        if cur_total_shares <= 0 {
            return Err(PoolError::InvalidAmount);
        }
        if total_assets < 0 {
            return Err(PoolError::InvalidAmount);
        }
        shares
            .checked_mul(total_assets)
            .and_then(|v| v.checked_div(cur_total_shares))
            .ok_or(PoolError::InvalidAmount)
    }

    fn assert_withdrawal_cooldown_elapsed(
        env: &Env,
        provider: &Address,
        token: &Address,
    ) -> Result<(), PoolError> {
        let cooldown = Self::withdrawal_cooldown(env);
        if cooldown == 0 {
            return Ok(());
        }

        let Some(deposit_ledger) = Self::read_deposit_timestamp(env, provider, token) else {
            return Ok(());
        };

        let current_ledger = env.ledger().sequence();
        if current_ledger < deposit_ledger.saturating_add(cooldown) {
            return Err(PoolError::WithdrawalCooldownActive);
        }

        Ok(())
    }

    /// Burns `shares` for `provider` and transfers out the proportional
    /// underlying assets, updating all pool accounting.
    ///
    /// This is the shared core of [`withdraw`](Self::withdraw) and
    /// [`emergency_withdraw`](Self::emergency_withdraw); it performs **no**
    /// pause or cooldown checks and emits **no** event. Callers are responsible
    /// for enforcing the appropriate policy and emitting the event that
    /// distinguishes a normal withdrawal from an emergency exit. On success it
    /// returns the amount of underlying assets transferred to `provider`.
    fn redeem_shares(
        env: &Env,
        provider: &Address,
        token: &Address,
        shares: i128,
    ) -> Result<i128, PoolError> {
        if shares <= 0 {
            return Err(PoolError::InvalidAmount);
        }

        let cur_shares = Self::read_shares(env, provider, token);
        if cur_shares < shares {
            return Err(PoolError::InsufficientBalance);
        }

        let cur_total_shares = Self::total_shares(env, token);
        // Redemption value is derived from internally-tracked managed assets, not
        // the raw token balance, so it cannot be manipulated by direct transfers
        // and does not swing while principal is out on loan (issue #2).
        let total_assets = Self::total_managed_assets(env, token);
        let assets_to_return = Self::calc_assets_to_redeem(shares, total_assets, cur_total_shares)?;

        if assets_to_return <= 0 {
            return Err(PoolError::InvalidAmount);
        }

        // Liquidity gate: the redemption must be serviceable from tokens the pool
        // physically holds. If principal is currently out on loan the share value
        // is unchanged, but the redemption is deferred rather than mis-priced.
        let liquid_balance = Self::read_pool_balance(env, token);
        if liquid_balance < assets_to_return {
            return Err(PoolError::InsufficientLiquidity);
        }

        // Principal portion being redeemed, used to keep TotalDeposits tracking
        // principal only (it must not absorb the yield portion of the payout).
        let cur_total_deposits = Self::total_deposits(env, token);
        let principal_redeemed = shares
            .checked_mul(cur_total_deposits)
            .and_then(|v| v.checked_div(cur_total_shares))
            .expect("principal redeem overflow");

        let share_key = DataKey::Shares(provider.clone(), token.clone());
        let deposit_key = DataKey::DepositTimestamp(provider.clone(), token.clone());
        let remaining = cur_shares.checked_sub(shares).expect("share underflow");
        if remaining == 0 {
            env.storage().persistent().remove(&share_key);
            env.storage().persistent().remove(&deposit_key);
            let count = Self::read_depositor_count(env, token);
            env.storage().instance().set(
                &DataKey::DepositorCount(token.clone()),
                &count.checked_sub(1).expect("depositor_count underflow"),
            );
        } else {
            env.storage().persistent().set(&share_key, &remaining);
            Self::bump_persistent_ttl(env, &share_key);
            Self::bump_persistent_ttl(env, &deposit_key);
        }

        let new_total_shares = cur_total_shares
            .checked_sub(shares)
            .expect("total shares underflow");
        env.storage()
            .instance()
            .set(&DataKey::TotalShares(token.clone()), &new_total_shares);

        // Managed assets shrink by the full payout (principal + yield portion).
        let new_managed_assets = total_assets
            .checked_sub(assets_to_return)
            .expect("managed assets underflow");
        Self::set_total_managed_assets(env, token, new_managed_assets);

        // TotalDeposits shrinks by the principal portion only, so it never drifts
        // away from actual net principal (issue #2, acceptance criterion 4).
        let new_total_deposits = cur_total_deposits.saturating_sub(principal_redeemed);
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits(token.clone()), &new_total_deposits);

        Self::bump_instance_ttl(env);

        // Interaction last (checks-effects-interactions): all accounting is
        // committed before the token leaves the pool.
        TokenClient::new(env, token).transfer(
            &env.current_contract_address(),
            provider,
            &assets_to_return,
        );

        Ok(assets_to_return)
    }

    // ── Admin / lifecycle ─────────────────────────────────────────────────

    pub fn initialize(env: Env, admin: Address) -> Result<(), PoolError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(PoolError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(
            &DataKey::WithdrawalCooldown,
            &Self::DEFAULT_WITHDRAWAL_COOLDOWN,
        );
        env.storage()
            .instance()
            .set(&DataKey::Version, &Self::CURRENT_VERSION);
        Self::bump_instance_ttl(&env);
        Ok(())
    }

    pub fn version(env: Env) -> u32 {
        Self::bump_instance_ttl(&env);
        env.storage().instance().get(&DataKey::Version).unwrap_or(0)
    }

    pub fn get_admin(env: Env) -> Address {
        Self::admin(&env)
    }

    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) {
        Self::admin(&env).require_auth();
        let old_version = Self::version(env.clone());
        let new_version = old_version.saturating_add(1);
        env.storage()
            .instance()
            .set(&DataKey::Version, &new_version);
        env.events().publish(
            (Symbol::new(&env, "ContractUpgraded"),),
            (old_version, new_version),
        );
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    /// Sets the maximum pool size cap.
    ///
    /// It is permitted to set a cap lower than the current `total_deposits`.
    /// When this occurs, the pool becomes "over cap" and all new deposits are
    /// blocked until withdrawals reduce `total_deposits` below the new cap.
    /// This is useful for safely winding down a pool without forcing withdrawals.
    pub fn set_max_pool_size(env: Env, token: Address, max: i128) -> Result<(), PoolError> {
        Self::admin(&env).require_auth();
        if max < 0 {
            return Err(PoolError::InvalidMaxPoolSize);
        }

        let old_max = Self::get_max_pool_size(env.clone(), token.clone());
        let current_deposits = Self::total_deposits(&env, &token);

        env.storage()
            .instance()
            .set(&DataKey::MaxPoolSize(token.clone()), &max);
        Self::bump_instance_ttl(&env);

        deposit_cap_updated(&env, token, old_max, max, current_deposits);
        Ok(())
    }

    pub fn set_withdrawal_cooldown(env: Env, ledgers: u32) -> Result<(), PoolError> {
        Self::admin(&env).require_auth();
        if ledgers > Self::MAX_WITHDRAWAL_COOLDOWN_LEDGERS {
            return Err(PoolError::CooldownTooLong);
        }

        let old_cooldown = Self::get_withdrawal_cooldown(env.clone());

        env.storage()
            .instance()
            .set(&DataKey::WithdrawalCooldown, &ledgers);
        Self::bump_instance_ttl(&env);

        withdrawal_cooldown_updated(&env, old_cooldown, ledgers);
        Ok(())
    }

    pub fn get_max_pool_size(env: Env, token: Address) -> i128 {
        Self::bump_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::MaxPoolSize(token))
            .unwrap_or(0)
    }

    pub fn is_over_cap(env: Env, token: Address) -> bool {
        let max = Self::get_max_pool_size(env.clone(), token.clone());
        if max == 0 {
            false
        } else {
            Self::total_deposits(&env, &token) > max
        }
    }

    pub fn get_total_deposits(env: Env, token: Address) -> i128 {
        Self::total_deposits(&env, &token)
    }

    pub fn get_total_shares(env: Env, token: Address) -> i128 {
        Self::total_shares(&env, &token)
    }

    /// Total underlying assets backing LP shares (principal + realized yield),
    /// i.e. the value the outstanding shares collectively redeem to. This is the
    /// accounting figure that drives the share price — distinct from
    /// `pool_balance` (raw on-hand tokens) and `get_total_deposits` (principal).
    pub fn get_total_managed_assets(env: Env, token: Address) -> i128 {
        Self::total_managed_assets(&env, &token)
    }

    pub fn get_withdrawal_cooldown(env: Env) -> u32 {
        Self::withdrawal_cooldown(&env)
    }

    // ── Core pool operations ──────────────────────────────────────────────

    /// Deposit `amount` of `token` and receive LP shares in return.
    ///
    /// Shares are minted proportional to the current exchange rate so that
    /// existing depositors are not diluted.  Any yield already present in the
    /// pool is captured in the share price at the point of deposit, not
    /// credited to the new depositor.
    pub fn deposit(
        env: Env,
        provider: Address,
        token: Address,
        amount: i128,
    ) -> Result<(), PoolError> {
        provider.require_auth();
        Self::assert_not_paused(&env)?;

        if amount <= 0 {
            return Err(PoolError::InvalidAmount);
        }

        // MaxPoolSize cap uses tracked principal, not pool balance.
        let max: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxPoolSize(token.clone()))
            .unwrap_or(0);
        if max > 0 {
            let total = Self::total_deposits(&env, &token);
            if total.checked_add(amount).expect("overflow") > max {
                return Err(PoolError::PoolSizeExceeded);
            }
        }

        // Snapshot pool state *before* the transfer so the share price reflects
        // the pre-deposit pool composition. Uses internally-tracked managed
        // assets (not the raw balance) so a direct transfer made just before the
        // deposit cannot dilute or inflate the minted shares (issue #2).
        let total_assets_before = Self::total_managed_assets(&env, &token);
        let cur_total_shares = Self::total_shares(&env, &token);

        // Issue #1: first depositor must commit at least MINIMUM_INITIAL_DEPOSIT
        // so the donation/inflation attack costs at least that much in real
        // assets before it could move the share price.
        if cur_total_shares == 0 && amount < Self::MINIMUM_INITIAL_DEPOSIT {
            return Err(PoolError::InvalidAmount);
        }

        let shares_to_mint =
            Self::calc_shares_to_mint(amount, total_assets_before, cur_total_shares);
        if shares_to_mint <= 0 {
            return Err(PoolError::InvalidAmount);
        }

        TokenClient::new(&env, &token).transfer(
            &provider,
            &env.current_contract_address(),
            &amount,
        );

        // Track new depositors.
        let existing_shares = Self::read_shares(&env, &provider, &token);
        if existing_shares == 0 {
            let count = Self::read_depositor_count(&env, &token);
            env.storage()
                .instance()
                .set(&DataKey::DepositorCount(token.clone()), &(count + 1));
        }

        let new_shares = existing_shares
            .checked_add(shares_to_mint)
            .expect("shares overflow");
        let share_key = DataKey::Shares(provider.clone(), token.clone());
        env.storage().persistent().set(&share_key, &new_shares);
        Self::bump_persistent_ttl(&env, &share_key);
        let deposit_key = DataKey::DepositTimestamp(provider.clone(), token.clone());
        let current_ledger = env.ledger().sequence();
        env.storage()
            .persistent()
            .set(&deposit_key, &current_ledger);
        Self::bump_persistent_ttl(&env, &deposit_key);

        let new_total_shares = cur_total_shares
            .checked_add(shares_to_mint)
            .expect("total shares overflow");
        env.storage()
            .instance()
            .set(&DataKey::TotalShares(token.clone()), &new_total_shares);

        let new_total_deposits = Self::total_deposits(&env, &token)
            .checked_add(amount)
            .expect("total deposits overflow");
        env.storage()
            .instance()
            .set(&DataKey::TotalDeposits(token.clone()), &new_total_deposits);

        // Deposited principal joins the managed-asset base 1:1.
        let new_managed_assets = total_assets_before
            .checked_add(amount)
            .expect("managed assets overflow");
        Self::set_total_managed_assets(&env, &token, new_managed_assets);

        Self::bump_instance_ttl(&env);
        deposit(
            &env,
            provider.clone(),
            token.clone(),
            amount,
            shares_to_mint,
        );
        Ok(())
    }

    /// Returns `(shares, current_asset_value)` for `provider` in the `token` pool.
    ///
    /// Net yield = `current_asset_value - original_deposit`.  Since original
    /// deposit amounts are not stored per-depositor, callers derive yield by
    /// comparing `current_asset_value` against their own recorded cost basis.
    pub fn get_depositor_yield(env: Env, provider: Address, token: Address) -> (i128, i128) {
        let shares = Self::read_shares(&env, &provider, &token);
        if shares == 0 {
            return (0, 0);
        }
        let cur_total_shares = Self::total_shares(&env, &token);
        if cur_total_shares == 0 {
            return (shares, 0);
        }
        let asset_value = Self::calc_assets_to_redeem(
            shares,
            Self::total_managed_assets(&env, &token),
            cur_total_shares,
        )
        .unwrap_or(0);
        (shares, asset_value)
    }

    /// Underlying asset value of `provider`'s LP shares (principal + yield).
    pub fn get_deposit(env: Env, provider: Address, token: Address) -> i128 {
        let shares = Self::read_shares(&env, &provider, &token);
        if shares == 0 {
            return 0;
        }
        let cur_total_shares = Self::total_shares(&env, &token);
        if cur_total_shares == 0 {
            return 0;
        }
        Self::calc_assets_to_redeem(
            shares,
            Self::total_managed_assets(&env, &token),
            cur_total_shares,
        )
        .unwrap_or(0)
    }

    /// Raw LP share balance for `provider` in the `token` pool.
    pub fn get_shares(env: Env, provider: Address, token: Address) -> i128 {
        Self::read_shares(&env, &provider, &token)
    }

    /// Current LP share price scaled by `SHARE_PRICE_SCALE`.
    /// `1_000_000` means 1.0 underlying asset per share.
    ///
    /// Derived from internally-tracked managed assets, so it is stable while
    /// principal is out on loan and immune to direct-transfer manipulation.
    pub fn get_share_price(env: Env, token: Address) -> i128 {
        let total_shares = Self::total_shares(&env, &token);
        if total_shares <= 0 {
            return Self::SHARE_PRICE_SCALE;
        }

        Self::total_managed_assets(&env, &token)
            .checked_mul(Self::SHARE_PRICE_SCALE)
            .and_then(|v| v.checked_div(total_shares))
            .expect("share price overflow")
    }

    /// Burn `shares` LP tokens and receive the proportional underlying assets.
    ///
    /// The redemption value is `shares * pool_balance / total_shares`, which
    /// automatically includes any interest that has been repaid to the pool
    /// since the shares were minted — no separate claim step is required.
    pub fn withdraw(
        env: Env,
        provider: Address,
        token: Address,
        shares: i128,
    ) -> Result<(), PoolError> {
        provider.require_auth();
        Self::assert_not_paused(&env)?;
        Self::assert_withdrawal_cooldown_elapsed(&env, &provider, &token)?;
        let assets = Self::redeem_shares(&env, &provider, &token, shares)?;
        withdraw(&env, provider, token, assets, shares);
        Ok(())
    }

    /// Emergency exit that lets a provider redeem their LP shares **only while
    /// the pool is paused**, bypassing the normal withdrawal cooldown.
    ///
    /// Policy: this function reverts with [`PoolError::NotPaused`] whenever the
    /// pool is not paused, so it is unavailable during normal operation. Pausing
    /// is an admin-only action ([`pause`](Self::pause)), so emergency exits can
    /// only happen in an admin-declared emergency state.
    ///
    /// The cooldown is deliberately bypassed here. The cooldown is an
    /// anti-gaming mechanism for *normal* operation; it must not trap depositors
    /// in a paused pool. Because emergency exits are confined to the paused
    /// state, they cannot be used to circumvent the cooldown during normal
    /// operation. Each call emits a distinct [`EmergencyWithdraw`] event
    /// (rather than the regular `Withdraw` event) so emergency exits are
    /// auditable and separable from ordinary withdrawals.
    pub fn emergency_withdraw(
        env: Env,
        provider: Address,
        token: Address,
        shares: i128,
    ) -> Result<(), PoolError> {
        provider.require_auth();
        if !Self::is_paused(env.clone()) {
            return Err(PoolError::NotPaused);
        }
        let assets = Self::redeem_shares(&env, &provider, &token, shares)?;
        emergency_withdraw(&env, provider, token, assets, shares);
        Ok(())
    }

    /// Authorize `loan_manager` to report realized yield for `token` via
    /// [`record_yield`](Self::record_yield). Set this to the LoanManager that
    /// repayments flow through so interest is credited to LPs automatically.
    /// Admin-gated.
    pub fn set_loan_manager(env: Env, token: Address, loan_manager: Address) {
        Self::admin(&env).require_auth();
        env.storage()
            .instance()
            .set(&DataKey::LoanManager(token.clone()), &loan_manager);
        Self::bump_instance_ttl(&env);
        loan_manager_updated(&env, token, loan_manager);
    }

    /// The address authorized to report yield for `token`, if any.
    pub fn get_loan_manager(env: Env, token: Address) -> Option<Address> {
        Self::loan_manager(&env, &token)
    }

    /// Record `amount` of realized yield (e.g. loan interest repaid into the
    /// pool), raising every outstanding share's value pro-rata.
    ///
    /// This is the *only* way assets enter share value besides deposits. Because
    /// a contract cannot distinguish a legitimate interest repayment from an
    /// unsolicited donation by looking at its balance, yield is recognised
    /// through this deliberate, access-gated call rather than by reading the raw
    /// balance. That is precisely what stops a direct transfer from arbitrarily
    /// changing existing holders' redeemable value (issue #2).
    ///
    /// Authorization: the configured [`LoanManager`] reporter for `token` (so the
    /// repayment path can credit interest automatically), or the admin when no
    /// reporter is configured (manual / keeper operation).
    ///
    /// Yield is not principal, so it does not move `TotalDeposits` or count
    /// against the `MaxPoolSize` cap. The caller is responsible for ensuring the
    /// corresponding tokens are actually present in the pool; otherwise later
    /// redemptions will hit the `InsufficientLiquidity` gate.
    pub fn record_yield(env: Env, token: Address, amount: i128) -> Result<(), PoolError> {
        // Gate on the configured yield reporter, falling back to admin so the
        // function is still usable manually before a LoanManager is wired up.
        match Self::loan_manager(&env, &token) {
            Some(reporter) => reporter.require_auth(),
            None => Self::admin(&env).require_auth(),
        }
        Self::assert_not_paused(&env)?;

        if amount <= 0 {
            return Err(PoolError::InvalidAmount);
        }

        // Yield is only meaningful once shares exist to distribute it to.
        if Self::total_shares(&env, &token) <= 0 {
            return Err(PoolError::InvalidAmount);
        }

        let new_managed_assets = Self::total_managed_assets(&env, &token)
            .checked_add(amount)
            .expect("managed assets overflow");
        Self::set_total_managed_assets(&env, &token, new_managed_assets);

        yield_distributed(&env, token, amount);
        Ok(())
    }

    // ── Queries ───────────────────────────────────────────────────────────

    pub fn get_pool_stats(env: Env, token: Address) -> PoolStats {
        let total_deposits = Self::total_deposits(&env, &token);
        let total_shares = Self::total_shares(&env, &token);
        let pool_token_balance = Self::read_pool_balance(&env, &token);

        // Utilisation: portion of tracked principal currently out on loan.
        let utilization_bps = if total_deposits > 0 && pool_token_balance < total_deposits {
            let borrowed = total_deposits - pool_token_balance;
            ((borrowed * 10_000) / total_deposits) as u32
        } else {
            0
        };

        PoolStats {
            total_deposits,
            total_shares,
            pool_token_balance,
            depositor_count: Self::read_depositor_count(&env, &token),
            utilization_bps,
        }
    }

    // ── Admin governance ──────────────────────────────────────────────────

    pub fn propose_admin(env: Env, new_admin: Address) {
        let current_admin = Self::admin(&env);
        current_admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::ProposedAdmin, &new_admin);
        Self::bump_instance_ttl(&env);

        admin_proposed(&env, current_admin.clone(), new_admin.clone());
    }

    pub fn accept_admin(env: Env) -> Result<(), PoolError> {
        let proposed_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::ProposedAdmin)
            .ok_or(PoolError::NoProposedAdmin)?;
        proposed_admin.require_auth();

        env.storage()
            .instance()
            .set(&DataKey::Admin, &proposed_admin);
        env.storage().instance().remove(&DataKey::ProposedAdmin);
        Self::bump_instance_ttl(&env);

        admin_transferred(&env, proposed_admin.clone());
        Ok(())
    }

    pub fn set_admin(env: Env, new_admin: Address) {
        let current_admin = Self::admin(&env);
        current_admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::ProposedAdmin);
        Self::bump_instance_ttl(&env);

        admin_transferred(&env, new_admin);
    }

    pub fn pause(env: Env) {
        Self::admin(&env).require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        Self::bump_instance_ttl(&env);

        pool_paused(&env);
    }

    pub fn unpause(env: Env) {
        Self::admin(&env).require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        Self::bump_instance_ttl(&env);

        pool_unpaused(&env);
    }

    pub fn is_paused(env: Env) -> bool {
        Self::bump_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    pub fn pool_balance(env: Env, token: Address) -> i128 {
        Self::read_pool_balance(&env, &token)
    }
}

#[cfg(test)]
mod test;
