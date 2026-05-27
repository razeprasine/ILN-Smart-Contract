//! Tests for Issue #59 (GovernanceProposal struct) and
//!           Issue #61 (cast_vote with anti-double-vote protection)

#![cfg(test)]

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{Client as TokenClient, StellarAssetClient},
    Address, BytesN, Env,
};

// ── Test helpers ──────────────────────────────────────────────────────────────

struct GovTestEnv {
    env: Env,
    contract: GovContractClient<'static>,
    gov_token: TokenClient<'static>,
    iln_contract: Address,
    voter_a: Address,
    voter_b: Address,
    proposer: Address,
}

fn setup() -> GovTestEnv {
    let env = Env::default();
    env.mock_all_auths();

    // Deploy a mock governance token.
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(token_admin.clone());
    let token_addr = token_id.address();

    let gov_token = TokenClient::new(&env, &token_addr);
    let gov_token_admin = StellarAssetClient::new(&env, &token_addr);

    let voter_a = Address::generate(&env);
    let voter_b = Address::generate(&env);
    let proposer = Address::generate(&env);

    // Mint governance tokens.
    gov_token_admin.mint(&voter_a, &1_000);
    gov_token_admin.mint(&voter_b, &2_000);
    gov_token_admin.mint(&proposer, &500);

    // Use a random address as the ILN contract stub (cross-contract calls are
    // not exercised in unit tests).
    let iln_contract = Address::generate(&env);

    let contract_id = env.register(GovContract, ());
    let contract = GovContractClient::new(&env, &contract_id);

    contract.initialize(&iln_contract, &token_addr).unwrap();

    let mut ledger = env.ledger().get();
    ledger.timestamp = 1_700_000_000;
    env.ledger().set(ledger);

    GovTestEnv { env, contract, gov_token, iln_contract, voter_a, voter_b, proposer }
}

fn dummy_hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[1u8; 32])
}

fn create_fee_proposal(t: &GovTestEnv) -> u64 {
    t.contract
        .create_proposal(
            &t.proposer,
            &ProposalAction::UpdateFeeRate(200),
            &dummy_hash(&t.env),
            &200_i128,
        )
        .unwrap()
}

// ── Issue #59: GovernanceProposal struct & storage ────────────────────────────

#[test]
fn test_create_proposal_stores_correct_fields() {
    let t = setup();
    let hash = dummy_hash(&t.env);
    let now = t.env.ledger().timestamp();

    let id = t
        .contract
        .create_proposal(
            &t.proposer,
            &ProposalAction::UpdateFeeRate(300),
            &hash,
            &300_i128,
        )
        .unwrap();

    let p = t.contract.get_proposal(&id).unwrap();

    assert_eq!(p.id, id);
    assert_eq!(p.proposer, t.proposer);
    assert_eq!(p.description_hash, hash);
    assert_eq!(p.action_type, ProposalAction::UpdateFeeRate(300));
    assert_eq!(p.proposed_value, 300);
    assert_eq!(p.status, ProposalStatus::Active);
    assert_eq!(p.votes_for, 0);
    assert_eq!(p.votes_against, 0);
    assert_eq!(p.created_at, now);
    assert_eq!(p.voting_end, now + 259_200);
}

#[test]
fn test_proposal_ids_increment() {
    let t = setup();
    let id1 = create_fee_proposal(&t);
    let id2 = create_fee_proposal(&t);
    assert_eq!(id2, id1 + 1);
}

#[test]
fn test_get_proposal_not_found_returns_error() {
    let t = setup();
    let result = t.contract.get_proposal(&9999);
    assert_eq!(result, Err(Ok(GovernanceError::ProposalNotFound)));
}

#[test]
fn test_proposal_action_add_token_stored_correctly() {
    let t = setup();
    let token_addr = Address::generate(&t.env);

    let id = t
        .contract
        .create_proposal(
            &t.proposer,
            &ProposalAction::AddToken(token_addr.clone()),
            &dummy_hash(&t.env),
            &0_i128,
        )
        .unwrap();

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.action_type, ProposalAction::AddToken(token_addr));
    assert_eq!(p.proposed_value, 0);
}

#[test]
fn test_proposal_action_remove_token_stored_correctly() {
    let t = setup();
    let token_addr = Address::generate(&t.env);

    let id = t
        .contract
        .create_proposal(
            &t.proposer,
            &ProposalAction::RemoveToken(token_addr.clone()),
            &dummy_hash(&t.env),
            &0_i128,
        )
        .unwrap();

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.action_type, ProposalAction::RemoveToken(token_addr));
}

#[test]
fn test_double_initialize_rejected() {
    let t = setup();
    let iln = Address::generate(&t.env);
    let token = Address::generate(&t.env);

    let result = t.contract.initialize(&iln, &token);
    assert_eq!(result, Err(Ok(GovernanceError::AlreadyInitialized)));
}

// ── Issue #61: cast_vote ──────────────────────────────────────────────────────

#[test]
fn test_cast_vote_for_updates_votes_for() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.votes_for, 1_000); // voter_a holds 1_000 tokens
    assert_eq!(p.votes_against, 0);
}

#[test]
fn test_cast_vote_against_updates_votes_against() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &false).unwrap();

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.votes_against, 1_000);
    assert_eq!(p.votes_for, 0);
}

#[test]
fn test_cast_vote_weight_equals_token_balance() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_b, &id, &true).unwrap();

    let p = t.contract.get_proposal(&id).unwrap();
    // voter_b holds 2_000 tokens.
    assert_eq!(p.votes_for, 2_000);
}

#[test]
fn test_multiple_voters_accumulate_correctly() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();
    t.contract.cast_vote(&t.voter_b, &id, &true).unwrap();

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.votes_for, 3_000); // 1_000 + 2_000
}

#[test]
fn test_has_voted_returns_true_after_vote() {
    let t = setup();
    let id = create_fee_proposal(&t);

    assert!(!t.contract.has_voted(&t.voter_a, &id));

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    assert!(t.contract.has_voted(&t.voter_a, &id));
}

// ── Issue #61: Anti-double-vote protection ────────────────────────────────────

#[test]
fn test_double_vote_rejected_with_already_voted_error() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    // Second vote from the same address must fail.
    let result = t.contract.cast_vote(&t.voter_a, &id, &false);
    assert_eq!(result, Err(Ok(GovernanceError::AlreadyVoted)));
}

#[test]
fn test_double_vote_does_not_change_vote_counts() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();
    // Attempt second vote — should fail.
    let _ = t.contract.cast_vote(&t.voter_a, &id, &false);

    let p = t.contract.get_proposal(&id).unwrap();
    // votes_against should still be 0.
    assert_eq!(p.votes_against, 0);
    assert_eq!(p.votes_for, 1_000);
}

#[test]
fn test_vote_on_nonexistent_proposal_rejected() {
    let t = setup();

    let result = t.contract.cast_vote(&t.voter_a, &9999, &true);
    assert_eq!(result, Err(Ok(GovernanceError::ProposalNotFound)));
}

#[test]
fn test_vote_after_voting_window_rejected() {
    let t = setup();
    let id = create_fee_proposal(&t);

    // Advance time past the 3-day voting window.
    let mut ledger = t.env.ledger().get();
    ledger.timestamp += 259_201;
    t.env.ledger().set(ledger);

    let result = t.contract.cast_vote(&t.voter_a, &id, &true);
    assert_eq!(result, Err(Ok(GovernanceError::VotingEnded)));
}

#[test]
fn test_voter_with_zero_balance_rejected() {
    let t = setup();
    let id = create_fee_proposal(&t);

    let zero_voter = Address::generate(&t.env);
    // zero_voter has no tokens minted.

    let result = t.contract.cast_vote(&zero_voter, &id, &true);
    assert_eq!(result, Err(Ok(GovernanceError::NoVotingPower)));
}

// ── Issue #61: VoteCast event ─────────────────────────────────────────────────

#[test]
fn test_cast_vote_emits_vote_cast_event() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    // Verify the VoteCast event was emitted by checking the contract emitted events.
    let events = t.env.events().all().filter_by_contract(&t.contract.address);
    // At least one event must have been emitted.
    assert!(!events.events().is_empty(), "VoteCast event should be emitted");
}

// ── execute_proposal integration ─────────────────────────────────────────────

#[test]
fn test_execute_before_voting_ends_fails() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    let result = t.contract.execute_proposal(&id, &10_000);
    assert_eq!(result, Err(Ok(GovernanceError::VotingOngoing)));
}

#[test]
fn test_execute_quorum_not_reached_rejected() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    // Advance past voting window.
    let mut ledger = t.env.ledger().get();
    ledger.timestamp += 259_201;
    t.env.ledger().set(ledger);

    // Total supply = 100_000; quorum = 10_000; voter_a voted 1_000 — below quorum.
    let result = t.contract.execute_proposal(&id, &100_000);
    assert_eq!(result, Err(Ok(GovernanceError::QuorumNotReached)));

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.status, ProposalStatus::Rejected);
}

#[test]
fn test_proposal_rejected_when_against_wins() {
    let t = setup();
    let id = create_fee_proposal(&t);

    // voter_a (1_000 for) vs voter_b (2_000 against).
    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();
    t.contract.cast_vote(&t.voter_b, &id, &false).unwrap();

    let mut ledger = t.env.ledger().get();
    ledger.timestamp += 259_201;
    t.env.ledger().set(ledger);

    // Total supply = 3_000 to meet quorum easily.
    let result = t.contract.execute_proposal(&id, &3_000);
    assert_eq!(result, Err(Ok(GovernanceError::ProposalRejected)));

    let p = t.contract.get_proposal(&id).unwrap();
    assert_eq!(p.status, ProposalStatus::Rejected);
}

#[test]
fn test_already_resolved_proposal_cannot_be_executed_again() {
    let t = setup();
    let id = create_fee_proposal(&t);

    t.contract.cast_vote(&t.voter_a, &id, &true).unwrap();

    let mut ledger = t.env.ledger().get();
    ledger.timestamp += 259_201;
    t.env.ledger().set(ledger);

    // First call fails quorum and sets status to Rejected.
    let _ = t.contract.execute_proposal(&id, &100_000);

    // Second call should return AlreadyResolved.
    let result = t.contract.execute_proposal(&id, &100_000);
    assert_eq!(result, Err(Ok(GovernanceError::AlreadyResolved)));
}
