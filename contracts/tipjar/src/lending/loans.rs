//! Loan lifecycle — open, repay, and interest accrual.
//!
//! Each borrower may have at most one active loan per token.  Collateral is
//! locked in the contract for the duration of the loan and released on full
//! repayment.  Interest accrues continuously using the utilisation-based rate
//! model from the `rates` module.

use soroban_sdk::{panic_with_error, token, Address, Env, Vec};

use crate::{DataKey, LendingError, LendingKey, LendingLoan, LoanStatus};

use super::{pool, rates, BPS_DENOM, HEALTH_FACTOR_PRECISION};

// ── Public API ───────────────────────────────────────────────────────────────

/// Open a new loan: lock `collateral_amount` and borrow `loan_amount` of `token`.
///
/// The borrower must not already have an active loan for this token.
/// Collateral must satisfy the configured LTV ratio.
/// Pool must have sufficient liquidity.
///
/// Emits `("lend_open",)` with `(loan_id, borrower, token, loan_amount, collateral_amount)`.
pub fn open_loan(
    env: &Env,
    borrower: &Address,
    token: &Address,
    loan_amount: i128,
    collateral_amount: i128,
) -> u64 {
    borrower.require_auth();

    pool::require_enabled(env);

    let config = rates::load_config(env)
        .unwrap_or_else(|| panic_with_error!(env, LendingError::NotConfigured));

    if !config.enabled {
        panic_with_error!(env, LendingError::Disabled);
    }

    if loan_amount < config.min_loan_amount {
        panic_with_error!(env, LendingError::LoanTooSmall);
    }

    if loan_amount <= 0 || collateral_amount <= 0 {
        panic_with_error!(env, LendingError::LoanTooSmall);
    }

    // Enforce LTV: loan_amount / collateral_amount <= max_ltv_bps / BPS_DENOM
    // i.e. loan_amount * BPS_DENOM <= collateral_amount * max_ltv_bps
    if loan_amount * BPS_DENOM as i128 > collateral_amount * config.max_ltv_bps as i128 {
        panic_with_error!(env, LendingError::LtvExceeded);
    }

    // Enforce collateral ratio: collateral >= loan * collateral_ratio_bps / BPS_DENOM
    if collateral_amount * BPS_DENOM as i128
        < loan_amount * config.collateral_ratio_bps as i128
    {
        panic_with_error!(env, LendingError::InsufficientCollateral);
    }

    // One active loan per borrower per token
    let borrower_loan_key =
        DataKey::Lending(LendingKey::BorrowerLoan(borrower.clone(), token.clone()));
    if env
        .storage()
        .persistent()
        .has(&borrower_loan_key)
    {
        panic_with_error!(env, LendingError::ActiveLoanExists);
    }

    // Pool must have enough liquidity
    let mut pool = pool::load_or_create_pool(env, token);
    let available = pool.total_deposits - pool.total_borrowed;
    if loan_amount > available {
        panic_with_error!(env, LendingError::InsufficientLiquidity);
    }

    // Lock collateral from borrower
    token::Client::new(env, token).transfer(
        borrower,
        &env.current_contract_address(),
        &collateral_amount,
    );

    // Assign loan ID
    let loan_id = next_loan_id(env);
    let now = env.ledger().timestamp();

    let loan = LendingLoan {
        loan_id,
        borrower: borrower.clone(),
        token: token.clone(),
        principal: loan_amount,
        collateral: collateral_amount,
        interest_accrued: 0,
        created_at: now,
        last_accrual: now,
        status: LoanStatus::Active,
    };

    // Persist loan
    env.storage().persistent().set(
        &DataKey::Lending(LendingKey::Loan(loan_id)),
        &loan,
    );

    // Map borrower → active loan
    env.storage()
        .persistent()
        .set(&borrower_loan_key, &loan_id);

    // Append to borrower's loan history
    track_borrower_loan(env, borrower, loan_id);

    // Update pool
    pool.total_borrowed += loan_amount;
    pool::save_pool(env, &pool);

    // Transfer loan amount to borrower
    token::Client::new(env, token).transfer(
        &env.current_contract_address(),
        borrower,
        &loan_amount,
    );

    env.events().publish(
        (soroban_sdk::symbol_short!("lend_opn"),),
        (loan_id, borrower.clone(), token.clone(), loan_amount, collateral_amount),
    );

    loan_id
}

/// Repay `repay_amount` toward an active loan.
///
/// Interest is accrued first, then the payment is applied: interest first,
/// then principal.  If the loan is fully repaid the collateral is returned
/// and the loan is marked `Repaid`.
///
/// Emits `("lend_rep",)` with `(loan_id, repay_amount, remaining_principal, interest_paid)`.
pub fn repay(env: &Env, repayer: &Address, loan_id: u64, repay_amount: i128) {
    repayer.require_auth();

    if repay_amount <= 0 {
        panic_with_error!(env, LendingError::LoanTooSmall);
    }

    let mut loan = load_loan_or_panic(env, loan_id);

    if loan.status != LoanStatus::Active {
        panic_with_error!(env, LendingError::LoanNotActive);
    }

    // Accrue interest up to now
    accrue_loan_interest(env, &mut loan);

    let total_owed = loan.principal + loan.interest_accrued;
    if repay_amount > total_owed {
        panic_with_error!(env, LendingError::RepayExceedsDebt);
    }

    // Transfer repayment from repayer into the contract
    token::Client::new(env, &loan.token).transfer(
        repayer,
        &env.current_contract_address(),
        &repay_amount,
    );

    // Apply payment: interest first, then principal
    let interest_paid;
    let principal_paid;
    if repay_amount <= loan.interest_accrued {
        interest_paid = repay_amount;
        principal_paid = 0;
        loan.interest_accrued -= repay_amount;
    } else {
        interest_paid = loan.interest_accrued;
        principal_paid = repay_amount - interest_paid;
        loan.interest_accrued = 0;
        loan.principal -= principal_paid;
    }

    // Distribute interest: protocol fee + lender share
    let config = rates::load_config(env)
        .unwrap_or_else(|| panic_with_error!(env, LendingError::NotConfigured));
    let protocol_fee = (interest_paid * config.protocol_fee_bps as i128) / BPS_DENOM as i128;
    let lender_interest = interest_paid - protocol_fee;

    // Update pool
    let mut pool = pool::load_or_create_pool(env, &loan.token);
    pool.total_borrowed -= principal_paid;
    pool.total_protocol_fees += protocol_fee;
    pool.total_interest_paid += lender_interest;
    // Lender interest stays in the pool as additional deposits
    pool.total_deposits += lender_interest;
    pool::save_pool(env, &pool);

    let fully_repaid = loan.principal == 0 && loan.interest_accrued == 0;

    if fully_repaid {
        loan.status = LoanStatus::Repaid;

        // Return collateral to borrower
        token::Client::new(env, &loan.token).transfer(
            &env.current_contract_address(),
            &loan.borrower,
            &loan.collateral,
        );

        // Remove active loan mapping
        env.storage().persistent().remove(&DataKey::Lending(
            LendingKey::BorrowerLoan(loan.borrower.clone(), loan.token.clone()),
        ));
    }

    save_loan(env, &loan);

    env.events().publish(
        (soroban_sdk::symbol_short!("lend_rep"),),
        (loan_id, repay_amount, loan.principal, interest_paid),
    );
}

/// Accrue interest on a loan without any payment.
///
/// Updates `loan.interest_accrued` and `loan.last_accrual` in-place and
/// persists the updated loan.  Safe to call at any time.
pub fn accrue(env: &Env, loan_id: u64) {
    let mut loan = load_loan_or_panic(env, loan_id);
    if loan.status != LoanStatus::Active {
        return;
    }
    accrue_loan_interest(env, &mut loan);
    save_loan(env, &loan);
}

/// Returns the loan record for `loan_id`, or `None`.
pub fn get_loan(env: &Env, loan_id: u64) -> Option<LendingLoan> {
    env.storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::Loan(loan_id)))
}

/// Returns the active loan ID for `(borrower, token)`, or `None`.
pub fn get_active_loan_id(env: &Env, borrower: &Address, token: &Address) -> Option<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::BorrowerLoan(
            borrower.clone(),
            token.clone(),
        )))
}

/// Returns all loan IDs ever opened by `borrower`.
pub fn get_borrower_loans(env: &Env, borrower: &Address) -> Vec<u64> {
    env.storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::BorrowerLoans(
            borrower.clone(),
        )))
        .unwrap_or_else(|| Vec::new(env))
}

/// Computes the health factor of a loan scaled by [`HEALTH_FACTOR_PRECISION`].
///
/// `health = (collateral * liquidation_threshold_bps * PRECISION) / ((principal + interest) * BPS_DENOM)`
///
/// Returns `i128::MAX` when there is no debt.
pub fn health_factor(env: &Env, loan: &LendingLoan) -> i128 {
    let total_debt = loan.principal + loan.interest_accrued;
    if total_debt == 0 {
        return i128::MAX;
    }

    let config = match rates::load_config(env) {
        Some(c) => c,
        None => return i128::MAX,
    };

    loan.collateral
        * config.liquidation_threshold_bps as i128
        * HEALTH_FACTOR_PRECISION
        / (total_debt * BPS_DENOM as i128)
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn accrue_loan_interest(env: &Env, loan: &mut LendingLoan) {
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

fn load_loan_or_panic(env: &Env, loan_id: u64) -> LendingLoan {
    env.storage()
        .persistent()
        .get(&DataKey::Lending(LendingKey::Loan(loan_id)))
        .unwrap_or_else(|| panic_with_error!(env, LendingError::LoanNotFound))
}

pub(super) fn save_loan(env: &Env, loan: &LendingLoan) {
    env.storage().persistent().set(
        &DataKey::Lending(LendingKey::Loan(loan.loan_id)),
        loan,
    );
}

fn track_borrower_loan(env: &Env, borrower: &Address, loan_id: u64) {
    let key = DataKey::Lending(LendingKey::BorrowerLoans(borrower.clone()));
    let mut ids: Vec<u64> = env
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Vec::new(env));
    ids.push_back(loan_id);
    env.storage().persistent().set(&key, &ids);
}

fn next_loan_id(env: &Env) -> u64 {
    let key = DataKey::Lending(LendingKey::LoanCtr);
    let current: u64 = env.storage().instance().get(&key).unwrap_or(0);
    let next = current + 1;
    env.storage().instance().set(&key, &next);
    next
}
