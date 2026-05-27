//! Tests for Issue #34 — Reputation-weighted LP priority queue
//!
//! Scenarios covered:
//!  - Single LP joins queue and resolves (happy path)
//!  - Highest-reputation LP wins when multiple LPs compete
//!  - Tie broken by first-come-first-served
//!  - Only the approved LP can fund after queue resolution
//!  - LP not in queue can still fund when no queue exists (backward compat)
//!  - Duplicate queue join rejected
//!  - join_fund_queue on non-existent invoice rejected
//!  - resolve_fund_queue on empty queue rejected
//!  - lp_score starts at neutral 50

#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env,
};

const INVOICE_AMOUNT: i128 = 1_000_000_000;
const DISCOUNT_RATE: u32 = 300;
const DUE_DATE_OFFSET: u64 = 60 * 60 * 24 * 30;

struct QueueTestEnv {
    env: Env,
    contract: InvoiceLiquidityContractClient<'static>,
    token: TokenClient<'static>,
    freelancer: Address,
    payer: Address,
    lp_a: Address,
    lp_b: Address,
    lp_c: Address,
}

fn setup_queue() -> QueueTestEnv {
    let env = Env::default();
    env.mock_all_auths();

    let usdc_admin = Address::generate(&env);
    let usdc_id = env.register_stellar_asset_contract_v2(usdc_admin.clone());
    let usdc_addr = usdc_id.address();

    let token = TokenClient::new(&env, &usdc_addr);
    let token_admin = StellarAssetClient::new(&env, &usdc_addr);

    let freelancer = Address::generate(&env);
    let payer = Address::generate(&env);
    let lp_a = Address::generate(&env);
    let lp_b = Address::generate(&env);
    let lp_c = Address::generate(&env);

    for lp in [&lp_a, &lp_b, &lp_c] {
        token_admin.mint(lp, &(INVOICE_AMOUNT * 10));
    }
    token_admin.mint(&payer, &(INVOICE_AMOUNT * 10));

    let contract_id = env.register(InvoiceLiquidityContract, ());
    let contract = InvoiceLiquidityContractClient::new(&env, &contract_id);
    token_admin.mint(&contract.address, &(INVOICE_AMOUNT * 100));

    let xlm_admin = Address::generate(&env);
    let xlm_id = env.register_stellar_asset_contract_v2(xlm_admin);
    let xlm_addr = xlm_id.address();

    contract.initialize(&usdc_admin, &usdc_addr, &xlm_addr);

    let mut ledger = env.ledger().get();
    ledger.timestamp = 1_700_000_000;
    env.ledger().set(ledger);

    QueueTestEnv { env, contract, token, freelancer, payer, lp_a, lp_b, lp_c }
}

fn submit_invoice(t: &QueueTestEnv) -> u64 {
    let due_date = t.env.ledger().timestamp() + DUE_DATE_OFFSET;
    t.contract.submit_invoice(
        &t.freelancer,
        &t.payer,
        &INVOICE_AMOUNT,
        &due_date,
        &DISCOUNT_RATE,
        &t.token.address,
    )
}

// ── lp_score ─────────────────────────────────────────────────────────────────

#[test]
fn test_lp_score_defaults_to_50() {
    let t = setup_queue();
    assert_eq!(t.contract.lp_score(&t.lp_a), 50);
}

// ── join_fund_queue ───────────────────────────────────────────────────────────

#[test]
fn test_single_lp_joins_queue_successfully() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    // No error means success; the LP is in the queue.
}

#[test]
fn test_duplicate_queue_join_rejected() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);

    let result = t.contract.try_join_fund_queue(&t.lp_a, &id);
    assert_eq!(result, Err(Ok(ContractError::AlreadyInQueue)));
}

#[test]
fn test_join_queue_nonexistent_invoice_fails() {
    let t = setup_queue();

    let result = t.contract.try_join_fund_queue(&t.lp_a, &999);
    assert_eq!(result, Err(Ok(ContractError::InvoiceNotFound)));
}

#[test]
fn test_join_queue_after_resolution_rejected() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    t.contract.resolve_fund_queue(&id);

    // Late arrival cannot join once queue is resolved.
    let result = t.contract.try_join_fund_queue(&t.lp_b, &id);
    assert_eq!(result, Err(Ok(ContractError::NotApprovedFunder)));
}

// ── resolve_fund_queue ────────────────────────────────────────────────────────

#[test]
fn test_resolve_queue_returns_only_lp_when_one_entry() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    let winner = t.contract.resolve_fund_queue(&id);

    assert_eq!(winner, t.lp_a);
}

#[test]
fn test_resolve_queue_empty_fails() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    let result = t.contract.try_resolve_fund_queue(&id);
    assert_eq!(result, Err(Ok(ContractError::NotFunded)));
}

#[test]
fn test_resolve_queue_is_idempotent() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    let first = t.contract.resolve_fund_queue(&id);
    let second = t.contract.resolve_fund_queue(&id);

    assert_eq!(first, second);
}

// ── Reputation ordering ───────────────────────────────────────────────────────

#[test]
fn test_highest_reputation_lp_wins_queue() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    // Manually boost lp_b's score by funding + getting paid on several invoices.
    // We do this by directly calling fund_invoice + mark_paid on extra invoices
    // to drive up lp_b's lp_score.

    // Simulate lp_b having a higher score than default by funding 3 invoices.
    for _ in 0..3u32 {
        let extra_id = submit_invoice(&t);
        t.contract.fund_invoice(&t.lp_b, &extra_id, &INVOICE_AMOUNT);
        // Each full fund adds 1 to lp_score → lp_b will be at 53.
    }

    // lp_a: score = 50 (default), lp_b: score = 53, lp_c: score = 50
    t.contract.join_fund_queue(&t.lp_a, &id);
    t.contract.join_fund_queue(&t.lp_b, &id);
    t.contract.join_fund_queue(&t.lp_c, &id);

    let winner = t.contract.resolve_fund_queue(&id);
    assert_eq!(winner, t.lp_b, "Highest-reputation LP should win");
}

#[test]
fn test_tie_broken_by_first_come_first_served() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    // lp_a and lp_b both have default score 50.
    t.contract.join_fund_queue(&t.lp_a, &id); // joins first
    t.contract.join_fund_queue(&t.lp_b, &id); // joins second

    let winner = t.contract.resolve_fund_queue(&id);
    // Tie → first in queue wins.
    assert_eq!(winner, t.lp_a, "First LP wins on tie");
}

// ── fund_invoice integration ──────────────────────────────────────────────────

#[test]
fn test_approved_lp_can_fund_after_queue_resolution() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    t.contract.resolve_fund_queue(&id);

    // lp_a is approved — should fund successfully.
    t.contract.fund_invoice(&t.lp_a, &id, &INVOICE_AMOUNT);

    let invoice = t.contract.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Funded);
}

#[test]
fn test_non_approved_lp_cannot_fund_after_queue_resolution() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    t.contract.resolve_fund_queue(&id);

    // lp_b is NOT the approved LP.
    let result = t.contract.try_fund_invoice(&t.lp_b, &id, &INVOICE_AMOUNT);
    assert_eq!(result, Err(Ok(ContractError::NotApprovedFunder)));
}

#[test]
fn test_fund_invoice_without_queue_works_normally() {
    // Backward-compatibility: if no queue is used, fund_invoice is first-come-first-served.
    let t = setup_queue();
    let id = submit_invoice(&t);

    // No queue join, no resolution — lp_a funds directly.
    t.contract.fund_invoice(&t.lp_a, &id, &INVOICE_AMOUNT);

    let invoice = t.contract.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Funded);
}

#[test]
fn test_lp_score_increases_after_successful_fund() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    let score_before = t.contract.lp_score(&t.lp_a);
    t.contract.fund_invoice(&t.lp_a, &id, &INVOICE_AMOUNT);
    let score_after = t.contract.lp_score(&t.lp_a);

    assert_eq!(score_after, score_before + 1);
}

#[test]
fn test_full_queue_lifecycle_with_payout() {
    let t = setup_queue();
    let id = submit_invoice(&t);

    t.contract.join_fund_queue(&t.lp_a, &id);
    t.contract.join_fund_queue(&t.lp_b, &id);

    let winner = t.contract.resolve_fund_queue(&id);
    // Both at score 50, lp_a wins tie.
    assert_eq!(winner, t.lp_a);

    t.contract.fund_invoice(&t.lp_a, &id, &INVOICE_AMOUNT);
    t.contract.mark_paid(&id);

    let invoice = t.contract.get_invoice(&id);
    assert_eq!(invoice.status, InvoiceStatus::Paid);
}
