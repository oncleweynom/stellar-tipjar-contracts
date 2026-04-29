//! Lending pool — lender deposit and withdrawal logic.
//!
//! Lenders deposit tokens into a per-token pool to provide liquidity for
//! borrowers.  Their share is tracked as a raw deposit amount (no LP tokens
//! in this implementation — simpler and gas-efficient for the tip use-case).
//! Interest earned is distributed pro-rata when borrowers repay.

use soroban_sdk::{panic_with_error, token, Address, Env, Vec};

use crate::{DataKey, LendingError, LendingKey, LendingPool};

use super::rates;

// ── Public API ───────────────────────────────────────────────────────────────

/// Deposit `amount` of `token` into the lending pool.
///
/// Transfers tokens from `lender` into the contract and records the deposit.
/// Creates the pool if it does not yet exist.
///
/// Emits `("lend_dep",)` with `(lender, token, amount, new_total_deposits)`.
pub fn deposit(env: &Env, lender: &Address, token: &Address, amount: i128) {
    lender.require_auth();

    if amount <= 0 {
        panic_with_error!(env, LendingError::InvalidDepositAmount);
    }

    require_enabled(env);

    // Transfer tokens from lender into the contract
    token::Client::new(env, token).transfer(lender, &env.current_contract_address(), &amount);

    // Update lender's deposit balance
    let deposit_key = DataKey::Lending(LendingKey::LenderDeposit(lender.clone(), token.clone()));
    let current: i128 = env.storage().persistent().get(&deposit_key).unwrap_or(0);
    env.storage()
        .persistent()
        .set(&deposit_key, &(current + amount));

    // Track which tokens this lender has deposited into
    track_lender_token(env, lender, token);

    // Update pool state
    let mut pool = load_or_create_pool(env, token);
    pool.total_deposits += amount;
    save_pool(env, &pool);

    env.events().publish(
        (soroban_sdk::symbol_short!("lend_dep"),),
        (lender.clone(), token.clone(), amount, pool.total_deposits),
    );
}

/// Withdraw `amount` of `token` from the lending pool.
///
/// Transfers tokens back to `lender`.  Panics if the withdrawal would leave
/// the pool unable to cover outstanding borrows.
///
/// Emits `("lend_wit",)` with `(lender, token, amount, new_total_deposits)`.
pub fn withdraw(env: &Env, lender: &Address, token: &Address, amount: i128) {
    lender.require_auth();

    if amount <= 0 {
        panic_with_error!(env, LendingError::InvalidDepositAmount);
    }

    require_enabled(env);

    let deposit_key = DataKey::Lending(LendingKey::LenderDeposit(lender.clone(), token.clone()));
    let current: i128 = env.storage().persistent().get(&deposit_key).unwrap_or(0);

    if amount > current {
        panic_with_error!(env, LendingError::WithdrawalExceedsDeposit);
    }

    let pool = load_pool(env, token).unwrap_or_else(|| panic_with_error!(env, LendingError::PoolNotFound));

    // Ensure pool remains solvent after withdrawal
    let available = pool.total_deposits - pool.total_borrowed;
    if amount > available {
        panic_with_error!(env, LendingError::InsufficientPoolLiquidity);
    }

    // Update lender deposit
    env.storage()
        .persistent()
        .set(&deposit_key, &(current - amount));

    // Update pool
    let mut updated_pool = pool.clone();
    updated_pool.total_deposits -= amount;
    save_pool(env, &updated_pool);

    // Transfer tokens back to lender
    token::Client::new(env, token).transfer(&env.current_contract_address(), lender, &amount);

    env.events().publish(
        (soroban_sdk::symbol_short!("lend_wit"),),
        (lender.clone(), token.clone(), amount, updated_pool.total_deposits),
    );
}

/// Returns the lender's deposit balance for `token`.
pub fn get_deposit(env: &Env, lender: &Address, token: &Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::LenderDeposit(
            lender.clone(),
            token.clone(),
        )))
        .unwrap_or(0)
}

/// Returns the pool state for `token`, or `None` if no pool exists.
pub fn get_pool(env: &Env, token: &Address) -> Option<LendingPool> {
    load_pool(env, token)
}

/// Returns the current utilisation ratio in basis points (0–10 000).
pub fn utilisation_bps(env: &Env, token: &Address) -> u32 {
    let pool = match load_pool(env, token) {
        Some(p) => p,
        None => return 0,
    };
    if pool.total_deposits <= 0 {
        return 0;
    }
    ((pool.total_borrowed * super::BPS_DENOM as i128) / pool.total_deposits)
        .min(super::BPS_DENOM as i128) as u32
}

// ── Internal helpers ─────────────────────────────────────────────────────────

pub(super) fn load_pool(env: &Env, token: &Address) -> Option<LendingPool> {
    env.storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::Pool(token.clone())))
}

pub(super) fn load_or_create_pool(env: &Env, token: &Address) -> LendingPool {
    load_pool(env, token).unwrap_or(LendingPool {
        token: token.clone(),
        total_deposits: 0,
        total_borrowed: 0,
        total_protocol_fees: 0,
        total_interest_paid: 0,
        last_accrual: env.ledger().timestamp(),
    })
}

pub(super) fn save_pool(env: &Env, pool: &LendingPool) {
    env.storage().persistent().set(
        &DataKey::Lending(LendingKey::Pool(pool.token.clone())),
        pool,
    );
}

fn track_lender_token(env: &Env, lender: &Address, token: &Address) {
    let key = DataKey::Lending(LendingKey::LenderTokens(lender.clone()));
    let mut tokens: Vec<Address> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    if !tokens.contains(token) {
        tokens.push_back(token.clone());
        env.storage().persistent().set(&key, &tokens);
    }
}

pub(super) fn require_enabled(env: &Env) {
    let enabled: bool = env
        .storage()
        .instance()
        .get(&DataKey::Lending(LendingKey::Enabled))
        .unwrap_or(true);
    if !enabled {
        panic_with_error!(env, LendingError::Disabled);
    }
}
