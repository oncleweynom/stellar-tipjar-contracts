#![cfg(test)]

extern crate std;

use soroban_sdk::{
    testutils::{Address as _, Ledger},
    Address, Env,
};
use tipjar::{LendingConfig, LendingError, LoanStatus, TipJarContract, TipJarContractClient};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn setup() -> (Env, TipJarContractClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, TipJarContract);
    let client = TipJarContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(token_admin.clone());

    client.init(&admin);
    client.add_token(&admin, &token_id);

    (env, client, admin, token_id)
}

/// Configure lending with sensible defaults and return the config.
fn configure_lending(client: &TipJarContractClient<'static>, admin: &Address) {
    client.lending_configure(
        admin,
        &15_000u32, // collateral_ratio_bps  150%
        &12_000u32, // liquidation_threshold_bps  120%
        &500u32,    // liquidation_penalty_bps  5%
        &500u32,    // base_rate_bps  5% p.a.
        &2_000u32,  // utilisation_multiplier_bps  20%
        &1_000u32,  // protocol_fee_bps  10%
        &100i128,   // min_loan_amount
        &6_667u32,  // max_ltv_bps  ~66.67%
    );
}

fn mint(env: &Env, token: &Address, to: &Address, amount: i128) {
    soroban_sdk::token::StellarAssetClient::new(env, token).mint(to, &amount);
}

// ── Configuration tests ───────────────────────────────────────────────────────

#[test]
fn test_configure_lending() {
    let (_, client, admin, _) = setup();
    configure_lending(&client, &admin);

    let config = client.lending_get_config().unwrap();
    assert_eq!(config.collateral_ratio_bps, 15_000);
    assert_eq!(config.liquidation_threshold_bps, 12_000);
    assert_eq!(config.liquidation_penalty_bps, 500);
    assert_eq!(config.base_rate_bps, 500);
    assert_eq!(config.max_ltv_bps, 6_667);
    assert!(config.enabled);
}

#[test]
#[should_panic]
fn test_configure_lending_invalid_ratio_panics() {
    let (_, client, admin, _) = setup();
    // liquidation_threshold_bps >= collateral_ratio_bps — invalid
    client.lending_configure(
        &admin,
        &12_000u32,
        &12_000u32, // equal — should panic
        &500u32,
        &500u32,
        &2_000u32,
        &1_000u32,
        &100i128,
        &6_667u32,
    );
}

// ── Pool deposit / withdraw tests ─────────────────────────────────────────────

#[test]
fn test_deposit_and_withdraw() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 10_000);

    client.lending_deposit(&lender, &token, &5_000i128);

    let pool = client.lending_get_pool(&token).unwrap();
    assert_eq!(pool.total_deposits, 5_000);
    assert_eq!(pool.total_borrowed, 0);
    assert_eq!(client.lending_get_deposit(&lender, &token), 5_000);

    client.lending_withdraw(&lender, &token, &2_000i128);

    let pool2 = client.lending_get_pool(&token).unwrap();
    assert_eq!(pool2.total_deposits, 3_000);
    assert_eq!(client.lending_get_deposit(&lender, &token), 3_000);
}

#[test]
#[should_panic]
fn test_withdraw_exceeds_deposit_panics() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 1_000);
    client.lending_deposit(&lender, &token, &1_000i128);

    // Try to withdraw more than deposited
    client.lending_withdraw(&lender, &token, &1_001i128);
}

// ── Loan lifecycle tests ──────────────────────────────────────────────────────

#[test]
fn test_open_and_repay_loan() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    // Provide liquidity
    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 100_000);
    client.lending_deposit(&lender, &token, &50_000i128);

    // Borrower opens a loan
    let borrower = Address::generate(&env);
    // collateral = 15_000, loan = 10_000 → ratio = 150% ✓, LTV = 66.67% ✓
    mint(&env, &token, &borrower, 15_000);
    let loan_id = client.lending_open_loan(&borrower, &token, &10_000i128, &15_000i128);

    let loan = client.lending_get_loan(&loan_id).unwrap();
    assert_eq!(loan.principal, 10_000);
    assert_eq!(loan.collateral, 15_000);
    assert!(matches!(loan.status, LoanStatus::Active));

    let pool = client.lending_get_pool(&token).unwrap();
    assert_eq!(pool.total_borrowed, 10_000);

    // Active loan ID is tracked
    assert_eq!(
        client.lending_get_active_loan_id(&borrower, &token),
        Some(loan_id)
    );

    // Repay full principal (no time elapsed so no interest)
    mint(&env, &token, &borrower, 10_000); // borrower needs tokens to repay
    client.lending_repay(&borrower, &loan_id, &10_000i128);

    let repaid_loan = client.lending_get_loan(&loan_id).unwrap();
    assert!(matches!(repaid_loan.status, LoanStatus::Repaid));
    assert_eq!(repaid_loan.principal, 0);

    // Active loan mapping cleared
    assert!(client
        .lending_get_active_loan_id(&borrower, &token)
        .is_none());

    // Pool updated
    let pool2 = client.lending_get_pool(&token).unwrap();
    assert_eq!(pool2.total_borrowed, 0);
}

#[test]
fn test_interest_accrual_on_repay() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 100_000);
    client.lending_deposit(&lender, &token, &50_000i128);

    let borrower = Address::generate(&env);
    mint(&env, &token, &borrower, 15_000);
    let loan_id = client.lending_open_loan(&borrower, &token, &10_000i128, &15_000i128);

    // Advance time by 365 days — at 5% base rate, interest ≈ 500
    env.ledger().with_mut(|li| li.timestamp += 365 * 24 * 3600);

    // Repay only interest portion (≈ 500)
    mint(&env, &token, &borrower, 1_000);
    client.lending_repay(&borrower, &loan_id, &500i128);

    let loan = client.lending_get_loan(&loan_id).unwrap();
    // After paying 500 toward interest, principal should still be 10_000
    // (interest_accrued ≈ 500, so after repay interest_accrued ≈ 0)
    assert_eq!(loan.principal, 10_000);
    assert!(matches!(loan.status, LoanStatus::Active));
}

#[test]
#[should_panic]
fn test_duplicate_active_loan_panics() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 100_000);
    client.lending_deposit(&lender, &token, &50_000i128);

    let borrower = Address::generate(&env);
    mint(&env, &token, &borrower, 30_000);

    client.lending_open_loan(&borrower, &token, &10_000i128, &15_000i128);
    // Second loan for same borrower/token should panic
    client.lending_open_loan(&borrower, &token, &5_000i128, &8_000i128);
}

#[test]
#[should_panic]
fn test_ltv_exceeded_panics() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 100_000);
    client.lending_deposit(&lender, &token, &50_000i128);

    let borrower = Address::generate(&env);
    // collateral = 10_000, loan = 9_000 → LTV = 90% > max_ltv 66.67% → panic
    mint(&env, &token, &borrower, 10_000);
    client.lending_open_loan(&borrower, &token, &9_000i128, &10_000i128);
}

// ── Liquidation tests ─────────────────────────────────────────────────────────

#[test]
fn test_liquidation_of_unhealthy_position() {
    let (env, client, admin, token) = setup();

    // Configure with very high rate to make position unhealthy quickly
    client.lending_configure(
        &admin,
        &12_001u32, // collateral_ratio_bps  ~120%
        &12_000u32, // liquidation_threshold_bps  120%
        &500u32,
        &50_000u32, // 500% p.a. — extreme rate to force unhealthy fast
        &50_000u32,
        &1_000u32,
        &1i128,
        &8_000u32, // max_ltv 80%
    );

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 1_000_000);
    client.lending_deposit(&lender, &token, &500_000i128);

    let borrower = Address::generate(&env);
    // collateral = 12_001, loan = 10_000 → ratio just above 120%
    mint(&env, &token, &borrower, 12_001);
    let loan_id = client.lending_open_loan(&borrower, &token, &10_000i128, &12_001i128);

    // Advance time — at 500% p.a. interest accrues rapidly
    // After ~90 days: interest ≈ 10_000 * 500% * 90/365 ≈ 12_329 → health < 1
    env.ledger().with_mut(|li| li.timestamp += 90 * 24 * 3600);

    // Trigger accrual so health factor is updated
    client.lending_accrue(&loan_id);

    let hf = client.lending_get_health_factor(&loan_id);
    assert!(hf < 1_000_000, "health factor should be below 1.0");

    // Liquidator repays the debt and receives collateral
    let liquidator = Address::generate(&env);
    let loan = client.lending_get_loan(&loan_id).unwrap();
    let total_debt = loan.principal + loan.interest_accrued;
    mint(&env, &token, &liquidator, total_debt + 1_000);

    client.lending_liquidate(&liquidator, &loan_id);

    let liquidated_loan = client.lending_get_loan(&loan_id).unwrap();
    assert!(matches!(liquidated_loan.status, LoanStatus::Liquidated));
    assert_eq!(liquidated_loan.principal, 0);
    assert_eq!(liquidated_loan.collateral, 0);
}

#[test]
#[should_panic]
fn test_liquidate_healthy_position_panics() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 100_000);
    client.lending_deposit(&lender, &token, &50_000i128);

    let borrower = Address::generate(&env);
    mint(&env, &token, &borrower, 15_000);
    let loan_id = client.lending_open_loan(&borrower, &token, &10_000i128, &15_000i128);

    // Position is healthy — liquidation should panic
    let liquidator = Address::generate(&env);
    mint(&env, &token, &liquidator, 20_000);
    client.lending_liquidate(&liquidator, &loan_id);
}

// ── Rate model tests ──────────────────────────────────────────────────────────

#[test]
fn test_utilisation_and_rate() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 100_000);
    client.lending_deposit(&lender, &token, &100_000i128);

    // No borrows → utilisation = 0, rate = base_rate = 500 bps
    assert_eq!(client.lending_get_utilisation(&token), 0);
    assert_eq!(client.lending_get_current_rate(&token), 500);

    let borrower = Address::generate(&env);
    mint(&env, &token, &borrower, 60_000);
    // Borrow 50_000 against 60_000 collateral (LTV = 50% < 66.67%)
    client.lending_open_loan(&borrower, &token, &50_000i128, &60_000i128);

    // Utilisation = 50_000 / 100_000 = 50% = 5_000 bps
    assert_eq!(client.lending_get_utilisation(&token), 5_000);

    // Rate = 500 + 5_000/10_000 * 2_000 = 500 + 1_000 = 1_500 bps
    assert_eq!(client.lending_get_current_rate(&token), 1_500);
}

// ── Loan history tests ────────────────────────────────────────────────────────

#[test]
fn test_borrower_loan_history() {
    let (env, client, admin, token) = setup();
    configure_lending(&client, &admin);

    let lender = Address::generate(&env);
    mint(&env, &token, &lender, 200_000);
    client.lending_deposit(&lender, &token, &200_000i128);

    let borrower = Address::generate(&env);
    mint(&env, &token, &borrower, 30_000);

    let loan_id_1 = client.lending_open_loan(&borrower, &token, &10_000i128, &15_000i128);

    // Repay first loan fully
    mint(&env, &token, &borrower, 10_000);
    client.lending_repay(&borrower, &loan_id_1, &10_000i128);

    // Open second loan
    mint(&env, &token, &borrower, 15_000);
    let loan_id_2 = client.lending_open_loan(&borrower, &token, &10_000i128, &15_000i128);

    let history = client.lending_get_borrower_loans(&borrower);
    assert_eq!(history.len(), 2);
    assert_eq!(history.get(0).unwrap(), loan_id_1);
    assert_eq!(history.get(1).unwrap(), loan_id_2);
}
