//! Liquidation logic for under-collateralised lending positions.
//!
//! Any caller can liquidate a loan whose health factor has dropped below 1.0.
//! The liquidator repays the full outstanding debt (principal + accrued interest)
//! and receives the collateral plus a penalty bonus.

use soroban_sdk::{panic_with_error, token, Address, Env};

use crate::{DataKey, LendingError, LendingKey, LoanStatus};

use super::{loans, pool, rates, BPS_DENOM, HEALTH_FACTOR_PRECISION};

// ── Public API ───────────────────────────────────────────────────────────────

/// Liquidate an under-collateralised loan.
///
/// The liquidator must repay the full outstanding debt (`principal + interest`).
/// In return they receive the entire collateral (which includes the penalty
/// bonus because the collateral ratio was > 100 % when the loan was opened).
///
/// Steps:
/// 1. Accrue interest to the current timestamp.
/// 2. Verify health factor < 1.0.
/// 3. Transfer `total_debt` from liquidator into the contract.
/// 4. Transfer full collateral to the liquidator.
/// 5. Mark loan as `Liquidated` and update pool state.
///
/// Emits `("lend_liq",)` with `(loan_id, liquidator, collateral_seized, debt_repaid)`.
pub fn liquidate(env: &Env, liquidator: &Address, loan_id: u64) {
    liquidator.require_auth();

    let mut loan: crate::LendingLoan = env
        .storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::Loan(loan_id)))
        .unwrap_or_else(|| panic_with_error!(env, LendingError::LoanNotFound));

    if loan.status != LoanStatus::Active {
        panic_with_error!(env, LendingError::LoanNotActive);
    }

    // Accrue interest up to now before computing health factor
    accrue_for_liquidation(env, &mut loan);

    // Health factor must be below 1.0
    let hf = loans::health_factor(env, &loan);
    if hf >= HEALTH_FACTOR_PRECISION {
        panic_with_error!(env, LendingError::PositionHealthy);
    }

    let total_debt = loan.principal + loan.interest_accrued;
    let collateral_seized = loan.collateral;

    // Transfer debt repayment from liquidator into the contract
    token::Client::new(env, &loan.token).transfer(
        liquidator,
        &env.current_contract_address(),
        &total_debt,
    );

    // Distribute interest portion: protocol fee + lender share
    let config = rates::load_config(env)
        .unwrap_or_else(|| panic_with_error!(env, LendingError::NotConfigured));
    let protocol_fee =
        (loan.interest_accrued * config.protocol_fee_bps as i128) / BPS_DENOM as i128;
    let lender_interest = loan.interest_accrued - protocol_fee;

    // Update pool
    let mut pool_state = pool::load_or_create_pool(env, &loan.token);
    pool_state.total_borrowed -= loan.principal;
    pool_state.total_protocol_fees += protocol_fee;
    pool_state.total_interest_paid += lender_interest;
    pool_state.total_deposits += lender_interest;
    pool::save_pool(env, &pool_state);

    // Mark loan liquidated and clear active mapping
    loan.status = LoanStatus::Liquidated;
    loan.principal = 0;
    loan.interest_accrued = 0;
    loan.collateral = 0;
    loans::save_loan(env, &loan);

    env.storage().persistent().remove(&DataKey::Lending(
        LendingKey::BorrowerLoan(loan.borrower.clone(), loan.token.clone()),
    ));

    // Transfer seized collateral to liquidator
    token::Client::new(env, &loan.token).transfer(
        &env.current_contract_address(),
        liquidator,
        &collateral_seized,
    );

    env.events().publish(
        (soroban_sdk::symbol_short!("lend_liq"),),
        (loan_id, liquidator.clone(), collateral_seized, total_debt),
    );
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn accrue_for_liquidation(env: &Env, loan: &mut crate::LendingLoan) {
    let now = env.ledger().timestamp();
    let elapsed = now.saturating_sub(loan.last_accrual);
    if elapsed == 0 {
        return;
    }

    let pool = pool::load_or_create_pool(env, &loan.token);
    let config = match rates::load_config(env) {
        Some(c) => c,
        None => return,
    };

    let rate_bps = rates::current_rate_bps(pool.total_deposits, pool.total_borrowed, &config);
    let interest = rates::accrue_interest(loan.principal, rate_bps, elapsed);
    loan.interest_accrued = loan.interest_accrued.saturating_add(interest);
    loan.last_accrual = now;
}
