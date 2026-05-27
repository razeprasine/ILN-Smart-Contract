//! ILN Governance Contract
//!
//! Issue #59 — GovernanceProposal struct with full spec fields.
//! Issue #61 — cast_vote() with anti-double-vote protection and VoteCast event.

#![no_std]
use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype,
    token::Client as TokenClient, vec, Address, BytesN, Env, IntoVal, Symbol, Vec,
};

// ================================================================
// Issue #59: Governance error enum
// ================================================================

#[contracterror]
#[derive(Clone, Debug, PartialEq)]
pub enum GovernanceError {
    /// Contract has already been initialised.
    AlreadyInitialized = 1,
    /// The requested proposal does not exist.
    ProposalNotFound = 2,
    /// The voting window has already closed.
    VotingEnded = 3,
    /// The proposal is no longer in Active status.
    ProposalNotActive = 4,
    /// The caller has no governance-token balance (no voting power).
    NoVotingPower = 5,
    /// Issue #61: The caller has already cast a vote on this proposal.
    AlreadyVoted = 6,
    /// The voting window is still open; cannot execute yet.
    VotingOngoing = 7,
    /// Quorum was not reached.
    QuorumNotReached = 8,
    /// Proposal was rejected (votes_against >= votes_for).
    ProposalRejected = 9,
    /// Proposal has already been resolved (Passed/Rejected/Executed).
    AlreadyResolved = 10,
}

// ================================================================
// Issue #59: ProposalAction — enum of all governable parameters
// ================================================================

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalAction {
    /// Update the protocol fee rate (basis points).
    UpdateFeeRate(u32),
    /// Approve a new token for invoice denomination.
    AddToken(Address),
    /// Remove a previously approved token.
    RemoveToken(Address),
    /// Update the maximum allowed discount rate (basis points).
    UpdateMaxDiscountRate(u32),
}

// ================================================================
// Issue #59: ProposalStatus
// ================================================================

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalStatus {
    Active,
    Passed,
    Rejected, // renamed from Failed per issue spec
    Executed,
}

// ================================================================
// Issue #59: GovernanceProposal struct — full spec
//
//  { id, proposer, description_hash, action_type, proposed_value,
//    status, votes_for, votes_against, created_at, voting_end }
// ================================================================

#[contracttype]
#[derive(Clone, Debug)]
pub struct GovernanceProposal {
    pub id: u64,
    /// Address that created the proposal.
    pub proposer: Address,
    /// SHA-256 hash of the human-readable description stored off-chain.
    pub description_hash: BytesN<32>,
    /// Which parameter this proposal intends to change.
    pub action_type: ProposalAction,
    /// Numeric value associated with the action (e.g. new fee rate in bps).
    /// For address-based actions (AddToken / RemoveToken) this is 0.
    pub proposed_value: i128,
    pub status: ProposalStatus,
    pub votes_for: i128,
    pub votes_against: i128,
    /// Ledger timestamp when the proposal was created.
    pub created_at: u64,
    /// Ledger timestamp when the voting window closes.
    pub voting_end: u64,
}

// ================================================================
// Issue #61: VoteCast event
// ================================================================

#[contractevent(topics = ["vote_cast"])]
#[derive(Clone, Debug, PartialEq)]
pub struct VoteCast {
    #[topic]
    pub proposal_id: u64,
    #[topic]
    pub voter: Address,
    /// true = voted for, false = voted against.
    pub support: bool,
    /// Voting weight (caller's token balance).
    pub weight: i128,
}

// ================================================================
// Storage keys
// ================================================================

#[contracttype]
pub enum StorageKey {
    IlnContract,
    GovToken,
    Proposal(u64),
    ProposalCount,
    /// Issue #61: per-(voter, proposal_id) double-vote guard.
    HasVoted(u64, Address),
}

// ================================================================
// Contract implementation
// ================================================================

#[contract]
pub struct GovContract;

#[contractimpl]
impl GovContract {
    // ── Initialise ────────────────────────────────────────────────

    pub fn initialize(
        env: Env,
        iln_contract: Address,
        gov_token: Address,
    ) -> Result<(), GovernanceError> {
        if env.storage().instance().has(&StorageKey::IlnContract) {
            return Err(GovernanceError::AlreadyInitialized);
        }
        env.storage()
            .instance()
            .set(&StorageKey::IlnContract, &iln_contract);
        env.storage()
            .instance()
            .set(&StorageKey::GovToken, &gov_token);
        env.storage()
            .instance()
            .set(&StorageKey::ProposalCount, &0_u64);
        Ok(())
    }

    // ── Issue #59: create_proposal ────────────────────────────────

    /// Create a new governance proposal.
    ///
    /// * `proposer`         – the address creating the proposal (must auth)
    /// * `action_type`      – which parameter to change
    /// * `description_hash` – SHA-256 of the off-chain description
    /// * `proposed_value`   – numeric value (0 for address-based actions)
    pub fn create_proposal(
        env: Env,
        proposer: Address,
        action_type: ProposalAction,
        description_hash: BytesN<32>,
        proposed_value: i128,
    ) -> Result<u64, GovernanceError> {
        proposer.require_auth();

        let count: u64 = env
            .storage()
            .instance()
            .get(&StorageKey::ProposalCount)
            .unwrap_or(0);
        let id = count + 1;

        let now = env.ledger().timestamp();
        // 3-day voting window.
        let voting_end = now + 259_200;

        let proposal = GovernanceProposal {
            id,
            proposer,
            description_hash,
            action_type,
            proposed_value,
            status: ProposalStatus::Active,
            votes_for: 0,
            votes_against: 0,
            created_at: now,
            voting_end,
        };

        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(id), &proposal);
        env.storage()
            .instance()
            .set(&StorageKey::ProposalCount, &id);

        Ok(id)
    }

    // ── Issue #61: cast_vote ──────────────────────────────────────

    /// Cast a vote on an active proposal.
    ///
    /// * `proposal_id` – the proposal to vote on
    /// * `support`     – true = vote for, false = vote against
    ///
    /// Vote weight equals the caller's current governance-token balance.
    /// Returns `GovernanceError::AlreadyVoted` if the caller has already voted.
    pub fn cast_vote(
        env: Env,
        voter: Address,
        proposal_id: u64,
        support: bool,
    ) -> Result<(), GovernanceError> {
        voter.require_auth();

        let mut proposal: GovernanceProposal = env
            .storage()
            .persistent()
            .get(&StorageKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        let now = env.ledger().timestamp();
        if now >= proposal.voting_end {
            return Err(GovernanceError::VotingEnded);
        }
        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::ProposalNotActive);
        }

        // ── Issue #61: Anti-double-vote protection ────────────────
        let voted_key = StorageKey::HasVoted(proposal_id, voter.clone());
        if env.storage().persistent().has(&voted_key) {
            return Err(GovernanceError::AlreadyVoted);
        }

        let token_addr: Address = env
            .storage()
            .instance()
            .get(&StorageKey::GovToken)
            .unwrap();
        let token = TokenClient::new(&env, &token_addr);
        let weight = token.balance(&voter);
        if weight == 0 {
            return Err(GovernanceError::NoVotingPower);
        }

        if support {
            proposal.votes_for += weight;
        } else {
            proposal.votes_against += weight;
        }

        // Record that this voter has voted (prevents double-voting).
        env.storage().persistent().set(&voted_key, &true);
        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(proposal_id), &proposal);

        // ── Issue #61: Emit VoteCast event ────────────────────────
        env.events().publish_event(&VoteCast {
            proposal_id,
            voter,
            support,
            weight,
        });

        Ok(())
    }

    // ── execute_proposal ─────────────────────────────────────────

    pub fn execute_proposal(
        env: Env,
        proposal_id: u64,
        total_supply: i128,
    ) -> Result<(), GovernanceError> {
        let mut proposal: GovernanceProposal = env
            .storage()
            .persistent()
            .get(&StorageKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)?;

        let now = env.ledger().timestamp();
        if now < proposal.voting_end {
            return Err(GovernanceError::VotingOngoing);
        }
        if proposal.status != ProposalStatus::Active {
            return Err(GovernanceError::AlreadyResolved);
        }

        let total_votes = proposal.votes_for + proposal.votes_against;
        let quorum = total_supply / 10; // 10% quorum

        if total_votes < quorum {
            proposal.status = ProposalStatus::Rejected;
            env.storage()
                .persistent()
                .set(&StorageKey::Proposal(proposal_id), &proposal);
            return Err(GovernanceError::QuorumNotReached);
        }

        if proposal.votes_for <= proposal.votes_against {
            proposal.status = ProposalStatus::Rejected;
            env.storage()
                .persistent()
                .set(&StorageKey::Proposal(proposal_id), &proposal);
            return Err(GovernanceError::ProposalRejected);
        }

        proposal.status = ProposalStatus::Passed;

        let iln_contract: Address = env
            .storage()
            .instance()
            .get(&StorageKey::IlnContract)
            .unwrap();

        match proposal.action_type.clone() {
            ProposalAction::UpdateFeeRate(rate) => {
                let args: Vec<soroban_sdk::Val> = vec![&env, rate.into_val(&env)];
                env.invoke_contract::<()>(
                    &iln_contract,
                    &Symbol::new(&env, "update_fee_rate"),
                    args,
                );
            }
            ProposalAction::AddToken(token) => {
                let args: Vec<soroban_sdk::Val> = vec![&env, token.into_val(&env)];
                env.invoke_contract::<()>(&iln_contract, &Symbol::new(&env, "add_token"), args);
            }
            ProposalAction::RemoveToken(token) => {
                let args: Vec<soroban_sdk::Val> = vec![&env, token.into_val(&env)];
                env.invoke_contract::<()>(
                    &iln_contract,
                    &Symbol::new(&env, "remove_token"),
                    args,
                );
            }
            ProposalAction::UpdateMaxDiscountRate(rate) => {
                let args: Vec<soroban_sdk::Val> = vec![&env, rate.into_val(&env)];
                env.invoke_contract::<()>(
                    &iln_contract,
                    &Symbol::new(&env, "update_max_discount"),
                    args,
                );
            }
        }

        proposal.status = ProposalStatus::Executed;
        env.storage()
            .persistent()
            .set(&StorageKey::Proposal(proposal_id), &proposal);

        Ok(())
    }

    // ── Getters ──────────────────────────────────────────────────

    /// Issue #59: get_proposal(id) → GovernanceProposal
    pub fn get_proposal(
        env: Env,
        proposal_id: u64,
    ) -> Result<GovernanceProposal, GovernanceError> {
        env.storage()
            .persistent()
            .get(&StorageKey::Proposal(proposal_id))
            .ok_or(GovernanceError::ProposalNotFound)
    }

    /// Return whether a specific address has already voted on a proposal.
    pub fn has_voted(env: Env, voter: Address, proposal_id: u64) -> bool {
        env.storage()
            .persistent()
            .has(&StorageKey::HasVoted(proposal_id, voter))
    }
}
