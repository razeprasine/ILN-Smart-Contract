use soroban_sdk::{contracttype, Address, BytesN, Env};

// ----------------------------------------------------------------
// Status enum — tracks lifecycle of invoice
// ----------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum InvoiceStatus {
    Pending,         // submitted, waiting for a liquidity provider to fund it
    Funded,          // LP has funded it, freelancer has been paid out
    PartiallyFunded, // partially funded by one or more LPs
    Paid,            // payer has settled in full, LP has been released
    Defaulted,       // past due_date and still unpaid
    Appealed,        // payer has contested the default ruling (issue #36)
    Expired,         // past due_date with no funding
    Cancelled,       // freelancer cancelled the invoice before funding
}

// ----------------------------------------------------------------
// Invoice struct (UPDATED - token stays per invoice)
// ----------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug)]
pub struct Invoice {
    pub id: u64,
    pub freelancer: Address, // who submitted the invoice (receives liquidity)
    pub payer: Address,      // the client who owes the money
    pub token: Address,      // token used for this invoice lifecycle
    pub amount: i128,        // full invoice value in stroops (1 USDC = 10_000_000)
    pub due_date: u64,       // Unix timestamp — when the payer must settle by
    pub discount_rate: u32,  // basis points, e.g. 300 = 3.00%
    pub status: InvoiceStatus,
    pub funder: Option<Address>, // set when an LP funds the invoice (legacy for full funding)
    pub funded_at: Option<u64>,  // ledger timestamp when funding occurred
    pub amount_funded: i128,     // cumulative amount funded so far
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct InvoiceParams {
    pub freelancer: Address,
    pub payer: Address,
    pub amount: i128,
    pub due_date: u64,
    pub discount_rate: u32,
    pub token: Address,
}

#[contracttype]
#[derive(Clone, Debug, Default)]
pub struct PayerStats {
    pub total_invoices: u64,
    pub paid_on_time: u64,
    pub defaults: u64,
    pub total_volume: i128,
}

#[contracttype]
#[derive(Clone, Debug, Default)]
pub struct ContractStats {
    pub total_invoices: u64,
    pub total_funded: u64,
    pub total_paid: u64,
    pub total_volume_usdc: i128,
    pub total_volume_eurc: i128,
    pub total_volume_xlm: i128,
}

// ----------------------------------------------------------------
// Issue #36: Appeal record stored per invoice
// ----------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug)]
pub struct AppealRecord {
    /// SHA-256 hash of off-chain evidence submitted by the payer.
    pub evidence_hash: BytesN<32>,
    /// Ledger timestamp when the appeal was filed.
    pub appealed_at: u64,
    /// Payer reputation score just before the default was applied,
    /// used to restore the score if the appeal is upheld.
    pub pre_default_score: u32,
}

// ----------------------------------------------------------------
// Issue #34: Single entry in the LP priority queue
// ----------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug)]
pub struct LpFundRequest {
    pub lp: Address,
    /// LP reputation score snapshotted at request time (used for ordering).
    pub score: u32,
}

// ----------------------------------------------------------------
// Storage key (UPDATED for multi-token registry + new features)
// ----------------------------------------------------------------

#[contracttype]
pub enum StorageKey {
    Invoice(u64),        // Invoice by ID
    InvoiceCount,        // auto-increment counter for IDs
    Token,               // USDC token address
    PayerScore(Address), // Payer reputation score
    InvoiceFunders(u64), // List of funders for a partially funded invoice
    ApprovedToken(Address),
    TokenList,
    Admin,
    FeeRate,
    MaxDiscountRate,
    DistributionContract,
    // ── Issue #36: appeal_default ──────────────────────────────────
    Appeal(u64),               // AppealRecord keyed by invoice ID
    PreDefaultPayerScore(u64), // payer score snapshot taken BEFORE claim_default penalty
    // ── Issue #34: LP priority queue ──────────────────────────────
    LpScore(Address),    // LP reputation score (distinct from PayerScore)
    FundQueue(u64),      // Vec<LpFundRequest> — LPs that joined the queue for an invoice
    QueueResolution(u64), // Address — the LP that won the priority queue
    // Contract stats counters
    TotalInvoices,       // Total invoices submitted
    TotalFunded,         // Total invoices fully funded
    TotalPaid,           // Total invoices paid
    TotalVolumeUsdc,     // Total volume in USDC
    TotalVolumeEurc,     // Total volume in EURC
    TotalVolumeXlm,      // Total volume in XLM
    // Pause/unpause
    Paused,              // Boolean flag for contract pause state
}

// ----------------------------------------------------------------
// Storage helpers — core invoice CRUD
// ----------------------------------------------------------------

pub fn save_invoice(env: &Env, invoice: &Invoice) {
    let key = StorageKey::Invoice(invoice.id);
    env.storage().persistent().set(&key, invoice);
    env.storage()
        .persistent()
        .extend_ttl(&key, 1_000_000, 2_000_000);
}

pub fn load_invoice(env: &Env, id: u64) -> Invoice {
    env.storage()
        .persistent()
        .get(&StorageKey::Invoice(id))
        .expect("invoice not found")
}

pub fn invoice_exists(env: &Env, id: u64) -> bool {
    env.storage().persistent().has(&StorageKey::Invoice(id))
}

pub fn next_invoice_id(env: &Env) -> u64 {
    let current: u64 = env
        .storage()
        .persistent()
        .get(&StorageKey::InvoiceCount)
        .unwrap_or(0);

    let next = current + 1;

    env.storage()
        .persistent()
        .set(&StorageKey::InvoiceCount, &next);

    next
}

// ----------------------------------------------------------------
// Payer reputation helpers
// ----------------------------------------------------------------

/// Get a payer's reputation score (0-100, default 50)
pub fn get_payer_score(env: &Env, payer: &Address) -> u32 {
    match env.storage()
        .persistent()
        .get::<StorageKey, ReputationScore>(&StorageKey::PayerScore(payer.clone()))
    {
        Some(mut rep) => {
            // Apply decay if enough ledgers have passed and config exists
            if let Ok(decay_config) = crate::config::get_config(env) {
                let current_ledger = env.ledger().sequence();
                let ledgers_since_activity = current_ledger.saturating_sub(rep.last_activity_ledger);
                
                if ledgers_since_activity >= decay_config.decay_period_ledgers 
                    && decay_config.decay_period_ledgers > 0 
                    && decay_config.decay_rate_bps > 0 
                {
                    // Calculate number of decay periods that have passed
                    let periods_passed = ledgers_since_activity / decay_config.decay_period_ledgers;
                    
                    // Apply decay: score = score * (1 - decay_rate/10000)^periods
                    let mut decayed_score = rep.score as u64;
                    for _ in 0..periods_passed {
                        // Decay: subtract decay_rate_bps basis points
                        let decay_amount = (decayed_score * decay_config.decay_rate_bps as u64) / 10_000;
                        decayed_score = decayed_score.saturating_sub(decay_amount);
                    }
                    
                    rep.score = (decayed_score.min(100)) as u32;
                }
            }
            
            rep.score
        }
        None => 50  // Default neutral score for new users
    }
}

/// Update a payer's reputation score (capped at 100)
pub fn set_payer_score(env: &Env, payer: &Address, score: u32) {
    let score = score.min(100);
    env.storage()
        .persistent()
        .set(&StorageKey::PayerScore(payer.clone()), &rep);
}

// ----------------------------------------------------------------
// Funder list helpers
// ----------------------------------------------------------------

/// Get the list of funders and their contributions for an invoice
pub fn get_invoice_funders(env: &Env, id: u64) -> soroban_sdk::Vec<(Address, i128)> {
    env.storage()
        .persistent()
        .get(&StorageKey::InvoiceFunders(id))
        .unwrap_or(soroban_sdk::Vec::new(env))
}

/// Save the list of funders for an invoice
pub fn save_invoice_funders(env: &Env, id: u64, funders: &soroban_sdk::Vec<(Address, i128)>) {
    env.storage()
        .persistent()
        .set(&StorageKey::InvoiceFunders(id), funders);
}

// ----------------------------------------------------------------
// Issue #36: Appeal helpers
// ----------------------------------------------------------------

pub fn get_appeal(env: &Env, invoice_id: u64) -> Option<AppealRecord> {
    env.storage()
        .persistent()
        .get(&StorageKey::Appeal(invoice_id))
}

pub fn save_appeal(env: &Env, invoice_id: u64, record: &AppealRecord) {
    env.storage()
        .persistent()
        .set(&StorageKey::Appeal(invoice_id), record);
}

/// Store the payer's score BEFORE the default penalty is applied.
/// Called inside claim_default() so appeal_default() can restore it later.
pub fn save_pre_default_payer_score(env: &Env, invoice_id: u64, score: u32) {
    env.storage()
        .persistent()
        .set(&StorageKey::PreDefaultPayerScore(invoice_id), &score);
}

pub fn get_pre_default_payer_score(env: &Env, invoice_id: u64) -> Option<u32> {
    env.storage()
        .persistent()
        .get(&StorageKey::PreDefaultPayerScore(invoice_id))
}

// ----------------------------------------------------------------
// Issue #34: LP score + queue helpers
// ----------------------------------------------------------------

/// LP reputation score starts at 50 (same neutral baseline as payers)
pub fn get_lp_score(env: &Env, lp: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&StorageKey::LpScore(lp.clone()))
        .unwrap_or(50)
}

/// Update an LP's reputation score (capped at 100)
pub fn set_lp_score(env: &Env, lp: &Address, score: u32) {
    let score = score.min(100);
    env.storage()
        .persistent()
        .set(&StorageKey::LpScore(lp.clone()), &score);
}

/// Return all queued LP requests for an invoice
pub fn get_fund_queue(env: &Env, invoice_id: u64) -> soroban_sdk::Vec<LpFundRequest> {
    env.storage()
        .persistent()
        .get(&StorageKey::FundQueue(invoice_id))
        .unwrap_or(soroban_sdk::Vec::new(env))
}

/// Persist the queue
pub fn save_fund_queue(env: &Env, invoice_id: u64, queue: &soroban_sdk::Vec<LpFundRequest>) {
    env.storage()
        .persistent()
        .set(&StorageKey::FundQueue(invoice_id), queue);
}

/// Return the resolved (approved) funder for an invoice, if any
pub fn get_queue_resolution(env: &Env, invoice_id: u64) -> Option<Address> {
    env.storage()
        .persistent()
        .get(&StorageKey::QueueResolution(invoice_id))
}

/// Store the approved funder chosen by the priority queue
pub fn save_queue_resolution(env: &Env, invoice_id: u64, approved_lp: &Address) {
    env.storage()
        .persistent()
        .set(&StorageKey::QueueResolution(invoice_id), approved_lp);
// Contract stats helpers
// ----------------------------------------------------------------

pub fn get_contract_stats(env: &Env) -> ContractStats {
    ContractStats {
        total_invoices: env
            .storage()
            .persistent()
            .get(&StorageKey::TotalInvoices)
            .unwrap_or(0),
        total_funded: env
            .storage()
            .persistent()
            .get(&StorageKey::TotalFunded)
            .unwrap_or(0),
        total_paid: env
            .storage()
            .persistent()
            .get(&StorageKey::TotalPaid)
            .unwrap_or(0),
        total_volume_usdc: env
            .storage()
            .persistent()
            .get(&StorageKey::TotalVolumeUsdc)
            .unwrap_or(0),
        total_volume_eurc: env
            .storage()
            .persistent()
            .get(&StorageKey::TotalVolumeEurc)
            .unwrap_or(0),
        total_volume_xlm: env
            .storage()
            .persistent()
            .get(&StorageKey::TotalVolumeXlm)
            .unwrap_or(0),
    }
}

pub fn increment_total_invoices(env: &Env) {
    let current: u64 = env
        .storage()
        .persistent()
        .get(&StorageKey::TotalInvoices)
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&StorageKey::TotalInvoices, &(current + 1));
}

pub fn increment_total_funded(env: &Env) {
    let current: u64 = env
        .storage()
        .persistent()
        .get(&StorageKey::TotalFunded)
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&StorageKey::TotalFunded, &(current + 1));
}

pub fn increment_total_paid(env: &Env) {
    let current: u64 = env
        .storage()
        .persistent()
        .get(&StorageKey::TotalPaid)
        .unwrap_or(0);
    env.storage()
        .persistent()
        .set(&StorageKey::TotalPaid, &(current + 1));
}

pub fn add_volume(env: &Env, token: &Address, amount: i128, usdc_addr: &Address, eurc_addr: &Address, xlm_addr: &Address) {
    if token == usdc_addr {
        let current: i128 = env
            .storage()
            .persistent()
            .get(&StorageKey::TotalVolumeUsdc)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&StorageKey::TotalVolumeUsdc, &(current + amount));
    } else if token == eurc_addr {
        let current: i128 = env
            .storage()
            .persistent()
            .get(&StorageKey::TotalVolumeEurc)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&StorageKey::TotalVolumeEurc, &(current + amount));
    } else if token == xlm_addr {
        let current: i128 = env
            .storage()
            .persistent()
            .get(&StorageKey::TotalVolumeXlm)
            .unwrap_or(0);
        env.storage()
            .persistent()
            .set(&StorageKey::TotalVolumeXlm, &(current + amount));
    }
}

pub fn is_paused(env: &Env) -> bool {
    env.storage()
        .instance()
        .get(&StorageKey::Paused)
        .unwrap_or(false)
}

pub fn set_paused(env: &Env, paused: bool) {
    env.storage().instance().set(&StorageKey::Paused, &paused);
}
