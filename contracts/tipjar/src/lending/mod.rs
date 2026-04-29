//! Peer-to-peer lending protocol for tip tokens.
//!
//! Lenders deposit tokens into per-token pools to earn interest.
//! Borrowers lock collateral (≥ `collateral_ratio_bps` of the loan) and
//! receive the borrowed tokens immediately.  Interest accrues continuously
//! using a utilisation-based rate model.  Under-collateralised positions can
//! be liquidated by any caller.
//!
//! # Module layout
//! - `pool`   — deposit / withdraw liquidity, pool state helpers
//! - `loans`  — open / repay loans, interest accrual
//! - `liquidation` — health-factor checks and liquidation logic
//! - `rates`  — interest-rate model

pub mod liquidation;
pub mod loans;
pub mod pool;
pub mod rates;

use soroban_sdk::{contracttype, Address};

// ── Basis-point constants ────────────────────────────────────────────────────

/// Basis-point denominator (10 000 = 100 %).
pub const BPS_DENOM: u32 = 10_000;

/// Seconds in 365 days — used for annualised rate calculations.
pub const SECONDS_PER_YEAR: u64 = 365 * 24 * 3600;

/// Health-factor precision multiplier (1 000 000 = 1.0).
pub const HEALTH_FACTOR_PRECISION: i128 = 1_000_000;

/// Default collateral ratio: 150 % (15 000 bps).
pub const DEFAULT_COLLATERAL_RATIO_BPS: u32 = 15_000;

/// Default liquidation threshold: 120 % (12 000 bps).
pub const DEFAULT_LIQUIDATION_THRESHOLD_BPS: u32 = 12_000;

/// Default liquidation penalty: 5 % (500 bps).
pub const DEFAULT_LIQUIDATION_PENALTY_BPS: u32 = 500;

/// Default base annual interest rate: 5 % (500 bps).
pub const DEFAULT_BASE_RATE_BPS: u32 = 500;

/// Default utilisation multiplier: 20 % (2 000 bps) extra at 100 % utilisation.
pub const DEFAULT_UTILISATION_MULTIPLIER_BPS: u32 = 2_000;

/// Default protocol fee on interest: 10 % (1 000 bps).
pub const DEFAULT_PROTOCOL_FEE_BPS: u32 = 1_000;

/// Default minimum loan amount.
pub const DEFAULT_MIN_LOAN_AMOUNT: i128 = 100;

/// Default max LTV: 66.67 % (6 667 bps).
pub const DEFAULT_MAX_LTV_BPS: u32 = 6_667;

// Re-export public types from lib.rs for use within this module.
pub use crate::{LendingConfig, LendingError, LendingLoan, LendingPool, LoanStatus};
