//! Interest-rate model for the lending protocol.
//!
//! Uses a two-slope utilisation model:
//!
//! ```text
//! rate = base_rate + utilisation * utilisation_multiplier
//! ```
//!
//! where `utilisation = total_borrowed / total_deposits` (clamped to [0, 1]).
//!
//! All rates are expressed in basis points per year.

use soroban_sdk::Env;

use crate::{DataKey, LendingConfig, LendingKey};

use super::{BPS_DENOM, SECONDS_PER_YEAR};

/// Computes the current annual interest rate in basis points for a pool.
///
/// Returns `base_rate_bps` when the pool is empty or has no borrows.
pub fn current_rate_bps(total_deposits: i128, total_borrowed: i128, config: &LendingConfig) -> u32 {
    if total_deposits <= 0 || total_borrowed <= 0 {
        return config.base_rate_bps;
    }

    // utilisation in bps (0–10 000)
    let utilisation_bps =
        ((total_borrowed * BPS_DENOM as i128) / total_deposits).min(BPS_DENOM as i128) as u32;

    // rate = base + utilisation_bps/BPS_DENOM * multiplier
    let extra = (utilisation_bps as u64 * config.utilisation_multiplier_bps as u64
        / BPS_DENOM as u64) as u32;

    config.base_rate_bps.saturating_add(extra)
}

/// Computes the interest accrued on `principal` over `elapsed_seconds` at
/// `rate_bps` per year.
///
/// `interest = principal * rate_bps * elapsed_seconds / (BPS_DENOM * SECONDS_PER_YEAR)`
pub fn accrue_interest(principal: i128, rate_bps: u32, elapsed_seconds: u64) -> i128 {
    if principal <= 0 || rate_bps == 0 || elapsed_seconds == 0 {
        return 0;
    }
    principal * rate_bps as i128 * elapsed_seconds as i128
        / (BPS_DENOM as i128 * SECONDS_PER_YEAR as i128)
}

/// Loads the lending config from instance storage.
pub fn load_config(env: &Env) -> Option<LendingConfig> {
    env.storage()
        .instance()
        .get(&DataKey::Lending(LendingKey::Config))
}
