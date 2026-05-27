use soroban_sdk::{contractevent, Address, BytesN};

use crate::invoice::InvoiceStatus;

#[contractevent(topics = ["submitted"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoiceSubmitted {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub freelancer: Address,
    #[topic]
    pub payer: Address,
    pub token: Address,
    pub amount: i128,
    pub due_date: u64,
    pub discount_rate: u32,
    pub status: InvoiceStatus,
    /// Ledger timestamp when the invoice was submitted.  Included so indexers
    /// can reconstruct the full invoice record from events alone.
    pub timestamp: u64,
}

#[contractevent(topics = ["updated"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoiceUpdated {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub freelancer: Address,
    #[topic]
    pub payer: Address,
    pub token: Address,
    pub amount: i128,
    pub due_date: u64,
    pub discount_rate: u32,
    pub status: InvoiceStatus,
}

#[contractevent(topics = ["funded"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoiceFunded {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub funder: Address,
    pub freelancer: Address,
    pub payer: Address,
    pub token: Address,
    pub fund_amount: i128,
    pub amount_funded: i128,
    pub invoice_amount: i128,
    pub due_date: u64,
    pub discount_rate: u32,
    pub funded_at: Option<u64>,
    pub status: InvoiceStatus,
}

#[contractevent(topics = ["paid"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoicePaid {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub payer: Address,
    pub funder: Address,
    pub freelancer: Address,
    pub token: Address,
    pub amount: i128,
    pub discount_amount: i128,
    pub due_date: u64,
    pub paid_on_time: bool,
    pub status: InvoiceStatus,
}

#[contractevent(topics = ["defaulted"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoiceDefaulted {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub funder: Address,
    pub freelancer: Address,
    pub payer: Address,
    pub token: Address,
    pub amount: i128,
    pub due_date: u64,
    pub defaulted_at: u64,
    pub discount_amount: i128,
    pub status: InvoiceStatus,
}

#[contractevent(topics = ["transferred"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoiceTransferred {
    #[topic]
    pub invoice_id: u64,
    pub old_freelancer: Address,
    pub new_freelancer: Address,
    pub status: InvoiceStatus,
}

#[contractevent(topics = ["cancelled"])]
#[derive(Clone, Debug, PartialEq)]
pub struct InvoiceCancelled {
    #[topic]
    pub invoice_id: u64,
    pub freelancer: Address,
    pub status: InvoiceStatus,
}

/// Emitted whenever the contract admin address is updated.
/// Provides a permanent on-chain audit trail for admin transitions.
#[contractevent(topics = ["admin_changed"])]
#[derive(Clone, Debug, PartialEq)]
pub struct AdminChanged {
    pub old_admin: Address,
    pub new_admin: Address,
    /// Ledger timestamp of the change.
    pub timestamp: u64,
}

// ── Issue #36: appeal_default events ──────────────────────────────────────────

/// Emitted when a payer files an appeal against an unfair default marking.
#[contractevent(topics = ["default_appealed"])]
#[derive(Clone, Debug, PartialEq)]
pub struct DefaultAppealed {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub payer: Address,
    /// SHA-256 hash of off-chain evidence provided by the payer.
    pub evidence_hash: BytesN<32>,
    pub appealed_at: u64,
}

/// Emitted when governance resolves a payer's appeal.
#[contractevent(topics = ["appeal_resolved"])]
#[derive(Clone, Debug, PartialEq)]
pub struct AppealResolved {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub payer: Address,
    /// true = appeal upheld (default reversed); false = appeal rejected.
    pub upheld: bool,
    pub resolved_at: u64,
}

// ── Issue #34: LP priority queue events ───────────────────────────────────────

/// Emitted when an LP registers their intent to fund via the priority queue.
#[contractevent(topics = ["fund_requested"])]
#[derive(Clone, Debug, PartialEq)]
pub struct FundRequested {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub lp: Address,
    /// LP's reputation score at the time of registration.
    pub score: u32,
}

/// Emitted when the priority queue is resolved and a winning LP is selected.
#[contractevent(topics = ["fund_queue_resolved"])]
#[derive(Clone, Debug, PartialEq)]
pub struct FundQueueResolved {
    #[topic]
    pub invoice_id: u64,
    #[topic]
    pub approved_lp: Address,
    /// Winning score that secured priority.
    pub score: u32,
}
