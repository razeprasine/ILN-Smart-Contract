#![no_std]

pub mod errors;
pub mod events;
pub mod invoice;
pub mod config;
pub mod rate_logic;
mod tests_regression;
mod tests_new_features;

pub use errors::ContractError;
pub use invoice::{Invoice, InvoiceParams, InvoiceStatus, ReputationScore};
pub use config::{Config, ConfigError, set_config, get_config};

use soroban_sdk::{
    contract, contractimpl, token::Client as TokenClient, vec, Address, BytesN, Env, IntoVal,
    Symbol, Vec,
};

use events::{
    AdminChanged, AppealResolved, DefaultAppealed, FundQueueResolved, FundRequested,
    InvoiceCancelled, InvoiceDefaulted, InvoiceFunded, InvoicePaid, InvoiceSubmitted,
    InvoiceTransferred, InvoiceUpdated,
};
use invoice::{
    get_appeal, get_fund_queue, get_invoice_funders, get_lp_score, get_payer_score,
    get_pre_default_payer_score, get_queue_resolution, invoice_exists, load_invoice,
    next_invoice_id, save_appeal, save_fund_queue, save_invoice, save_invoice_funders,
    save_pre_default_payer_score, save_queue_resolution, set_lp_score, set_payer_score,
    AppealRecord, LpFundRequest, StorageKey,
};

// 30-day window in seconds for a payer to file an appeal after a default.
const APPEAL_WINDOW_SECONDS: u64 = 30 * 24 * 60 * 60;

// ----------------------------------------------------------------
// CONSTANTS
// ----------------------------------------------------------------

/// Minimum invoice duration: 24 hours (in seconds)
const MIN_INVOICE_DURATION: u64 = 24 * 60 * 60;

/// Maximum invoice duration: 365 days (in seconds)
const MAX_INVOICE_DURATION: u64 = 365 * 24 * 60 * 60;

// ----------------------------------------------------------------
// CONTRACT
// ----------------------------------------------------------------

#[contract]
pub struct InvoiceLiquidityContract;

#[contractimpl]
impl InvoiceLiquidityContract {
    // ------------------------------------------------------------
    // initialize (multi-token aware)
    // ------------------------------------------------------------
    pub fn initialize(
        env: Env,
        admin: Address,
        token: Address,
        xlm_token: Address,
    ) -> Result<(), ContractError> {
        if env.storage().instance().has(&StorageKey::InvoiceCount) {
            return Err(ContractError::AlreadyInitialized);
        }

        env.storage().instance().set(&StorageKey::Admin, &admin);
        env.storage().instance().set(&StorageKey::FeeRate, &0_u32);
        env.storage()
            .instance()
            .set(&StorageKey::MaxDiscountRate, &5000_u32);

        // approve first token (USDC or default)
        env.storage()
            .persistent()
            .set(&StorageKey::ApprovedToken(token.clone()), &true);

        // approve native XLM SAC
        env.storage()
            .persistent()
            .set(&StorageKey::ApprovedToken(xlm_token.clone()), &true);

        let mut list: Vec<Address> = Vec::new(&env);
        list.push_back(token.clone());
        list.push_back(xlm_token.clone());

        env.storage()
            .persistent()
            .set(&StorageKey::TokenList, &list);

        Ok(())
    }

    // ------------------------------------------------------------
    pub fn set_admin(env: Env, new_admin: Address) {
        let old_admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        old_admin.require_auth();
        env.storage().instance().set(&StorageKey::Admin, &new_admin);
        env.events().publish_event(&AdminChanged {
            old_admin,
            new_admin,
            timestamp: env.ledger().timestamp(),
        });
    }

    pub fn update_fee_rate(env: Env, rate: u32) {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        env.storage().instance().set(&StorageKey::FeeRate, &rate);
    }

    pub fn update_max_discount(env: Env, rate: u32) {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&StorageKey::MaxDiscountRate, &rate);
    }

    pub fn set_distribution_contract(env: Env, distribution_contract: Address) {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .instance()
            .set(&StorageKey::DistributionContract, &distribution_contract);
    }

    pub fn add_token(env: Env, token: Address) {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .persistent()
            .set(&StorageKey::ApprovedToken(token.clone()), &true);

        let mut list: Vec<Address> = env
            .storage()
            .persistent()
            .get(&StorageKey::TokenList)
            .unwrap_or(Vec::new(&env));
        if !list.contains(&token) {
            list.push_back(token);
            env.storage()
                .persistent()
                .set(&StorageKey::TokenList, &list);
        }
    }

    pub fn remove_token(env: Env, token: Address) {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        env.storage()
            .persistent()
            .set(&StorageKey::ApprovedToken(token.clone()), &false);
    }

    // ------------------------------------------------------------
    // pause / unpause (emergency controls)
    // ------------------------------------------------------------
    pub fn pause(env: Env) -> Result<(), ContractError> {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        set_paused(&env, true);
        env.events().publish_event(&ContractPaused {
            timestamp: env.ledger().timestamp(),
        });
        Ok(())
    }

    pub fn unpause(env: Env) -> Result<(), ContractError> {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();
        set_paused(&env, false);
        env.events().publish_event(&ContractUnpaused {
            timestamp: env.ledger().timestamp(),
        });
        Ok(())
    }

    // ------------------------------------------------------------
    // get_contract_stats (read-only view)
    // ------------------------------------------------------------
    pub fn get_contract_stats(env: Env) -> ContractStats {
        get_contract_stats(&env)
    }

    // ------------------------------------------------------------
    // submit_invoice (NOW TOKEN-AWARE)
    // ------------------------------------------------------------
    pub fn submit_invoice(
        env: Env,
        freelancer: Address,
        payer: Address,
        amount: i128,
        due_date: u64,
        discount_rate: u32,
        token: Address,
    ) -> Result<u64, ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        freelancer.require_auth();

        validate_invoice_terms(&env, amount, due_date, discount_rate)?;

        // token validation
        if !is_approved_token(&env, &token) {
            return Err(ContractError::Unauthorized);
        }

        let id = next_invoice_id(&env);

        let invoice = Invoice {
            id,
            freelancer,
            payer,
            token,
            amount,
            due_date,
            discount_rate,
            status: InvoiceStatus::Pending,
            funder: None,
            funded_at: None,
            amount_funded: 0,
        };

        save_invoice(&env, &invoice);

        // Increment total invoices counter
        increment_total_invoices(&env);

        env.events().publish_event(&InvoiceSubmitted {
            invoice_id: invoice.id,
            freelancer: invoice.freelancer.clone(),
            payer: invoice.payer.clone(),
            token: invoice.token.clone(),
            amount: invoice.amount,
            due_date: invoice.due_date,
            discount_rate: invoice.discount_rate,
            status: invoice.status.clone(),
            timestamp: env.ledger().timestamp(),
        });

        Ok(id)
    }

    // ------------------------------------------------------------
    // update_invoice
    // ------------------------------------------------------------
    pub fn update_invoice(
        env: Env,
        freelancer: Address,
        invoice_id: u64,
        amount: i128,
        due_date: u64,
        discount_rate: u32,
    ) -> Result<(), ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);
        freelancer.require_auth();

        if invoice.freelancer != freelancer {
            return Err(ContractError::Unauthorized);
        }

        if invoice.status == InvoiceStatus::Pending && env.ledger().timestamp() >= invoice.due_date
        {
            invoice.status = InvoiceStatus::Expired;
            save_invoice(&env, &invoice);
            return Err(ContractError::InvoiceExpired);
        }

        match invoice.status {
            InvoiceStatus::Pending => {}
            InvoiceStatus::PartiallyFunded | InvoiceStatus::Funded => {
                return Err(ContractError::AlreadyFunded)
            }
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        validate_invoice_terms(&env, amount, due_date, discount_rate)?;

        invoice.amount = amount;
        invoice.due_date = due_date;
        invoice.discount_rate = discount_rate;

        save_invoice(&env, &invoice);

        env.events().publish_event(&InvoiceUpdated {
            invoice_id: invoice.id,
            freelancer: invoice.freelancer.clone(),
            payer: invoice.payer.clone(),
            token: invoice.token.clone(),
            amount: invoice.amount,
            due_date: invoice.due_date,
            discount_rate: invoice.discount_rate,
            status: invoice.status.clone(),
        });

        Ok(())
    }

    // ------------------------------------------------------------
    // submit_invoices_batch
    // ------------------------------------------------------------
    pub fn submit_invoices_batch(
        env: Env,
        invoices: Vec<InvoiceParams>,
    ) -> Result<Vec<u64>, ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        if invoices.len() > 10 {
            return Err(ContractError::BatchTooLarge);
        }

        let mut authenticated_freelancers: Vec<Address> = Vec::new(&env);
        let mut ids = Vec::new(&env);
        for params in invoices.iter() {
            if !authenticated_freelancers.contains(&params.freelancer) {
                params.freelancer.require_auth();
                authenticated_freelancers.push_back(params.freelancer.clone());
            }

            validate_invoice_terms(&env, params.amount, params.due_date, params.discount_rate)?;

            if !is_approved_token(&env, &params.token) {
                return Err(ContractError::Unauthorized);
            }

            let id = next_invoice_id(&env);

            let invoice = Invoice {
                id,
                freelancer: params.freelancer,
                payer: params.payer,
                token: params.token,
                amount: params.amount,
                due_date: params.due_date,
                discount_rate: params.discount_rate,
                status: InvoiceStatus::Pending,
                funder: None,
                funded_at: None,
                amount_funded: 0,
            };

            save_invoice(&env, &invoice);

            // Increment total invoices counter
            increment_total_invoices(&env);

            env.events().publish_event(&InvoiceSubmitted {
                invoice_id: invoice.id,
                freelancer: invoice.freelancer.clone(),
                payer: invoice.payer.clone(),
                token: invoice.token.clone(),
                amount: invoice.amount,
                due_date: invoice.due_date,
                discount_rate: invoice.discount_rate,
                status: invoice.status.clone(),
                timestamp: env.ledger().timestamp(),
            });

            ids.push_back(id);
        }

        Ok(ids)
    }

    // ================================================================
    // Issue #34: LP Priority Queue
    //
    // Design:
    //  1. Any LP calls `join_fund_queue(lp, invoice_id)` to register intent.
    //     Their current LP reputation score is snapshotted.
    //  2. Anyone can call `resolve_fund_queue(invoice_id)` to lock in the
    //     highest-score LP as the approved funder.
    //  3. `fund_invoice` checks: if a QueueResolution exists for this invoice,
    //     only the approved LP may fund it.
    //  If no LP ever joins the queue the existing first-come-first-served
    //  behaviour is preserved unchanged.
    // ================================================================

    /// Register an LP's intent to fund an invoice.
    /// The LP's current reputation score is snapshotted for ordering.
    pub fn join_fund_queue(
        env: Env,
        lp: Address,
        invoice_id: u64,
    ) -> Result<(), ContractError> {
        lp.require_auth();

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        // Queue resolution already happened — too late to join.
        if get_queue_resolution(&env, invoice_id).is_some() {
            return Err(ContractError::NotApprovedFunder);
        }

        let invoice = load_invoice(&env, invoice_id);
        match invoice.status {
            InvoiceStatus::Pending | InvoiceStatus::PartiallyFunded => {}
            InvoiceStatus::Funded => return Err(ContractError::AlreadyFunded),
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        let mut queue = get_fund_queue(&env, invoice_id);

        // Prevent duplicate entries.
        for i in 0..queue.len() {
            if queue.get(i).unwrap().lp == lp {
                return Err(ContractError::AlreadyInQueue);
            }
        }

        let score = get_lp_score(&env, &lp);
        queue.push_back(LpFundRequest { lp: lp.clone(), score });
        save_fund_queue(&env, invoice_id, &queue);

        env.events().publish_event(&FundRequested { invoice_id, lp, score });

        Ok(())
    }

    /// Select the highest-reputation LP from the queue as the approved funder.
    /// Returns the winning LP address.
    /// Can be called by anyone once at least one LP has joined the queue.
    pub fn resolve_fund_queue(
        env: Env,
        invoice_id: u64,
    ) -> Result<Address, ContractError> {
        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        // Already resolved.
        if let Some(approved) = get_queue_resolution(&env, invoice_id) {
            return Ok(approved);
        }

        let queue = get_fund_queue(&env, invoice_id);
        if queue.is_empty() {
            return Err(ContractError::NotFunded); // no one in queue
        }

        // Find the LP with the highest score (ties broken by first-come-first-served).
        let mut best_lp = queue.get(0).unwrap().lp.clone();
        let mut best_score = queue.get(0).unwrap().score;

        for i in 1..queue.len() {
            let entry = queue.get(i).unwrap();
            if entry.score > best_score {
                best_score = entry.score;
                best_lp = entry.lp.clone();
            }
        }

        save_queue_resolution(&env, invoice_id, &best_lp);

        env.events().publish_event(&FundQueueResolved {
            invoice_id,
            approved_lp: best_lp.clone(),
            score: best_score,
        });

        Ok(best_lp)
    }

    // ------------------------------------------------------------
    // fund_invoice (USES invoice.token) — now queue-aware
    // ------------------------------------------------------------
    pub fn fund_invoice(
        env: Env,
        funder: Address,
        invoice_id: u64,
        fund_amount: i128,
    ) -> Result<(), ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        funder.require_auth();

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        // ── Issue #34: priority queue check ──────────────────────
        // If a queue has been resolved, only the approved LP may fund.
        if let Some(approved) = get_queue_resolution(&env, invoice_id) {
            if approved != funder {
                return Err(ContractError::NotApprovedFunder);
            }
        }

        let mut invoice = load_invoice(&env, invoice_id);

        if invoice.status == InvoiceStatus::Pending && env.ledger().timestamp() >= invoice.due_date
        {
            invoice.status = InvoiceStatus::Expired;
            save_invoice(&env, &invoice);
            return Err(ContractError::InvoiceExpired);
        }

        match invoice.status {
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Funded => return Err(ContractError::AlreadyFunded),
            InvoiceStatus::Pending | InvoiceStatus::PartiallyFunded => {} // all good
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        if invoice.amount_funded + fund_amount > invoice.amount {
            return Err(ContractError::OverfundingRejected);
        }

        // --- Execute transfer ---
        let token = token_client(&env, &invoice.token);
        let contract_address = env.current_contract_address();

        let fund_discount = fund_amount
            .checked_mul(discount_rate_as_i128(invoice.discount_rate))
            .unwrap_or(0)
            / 10_000;
        let cost = fund_amount - fund_discount;

        token.transfer(&funder, &contract_address, &cost);

        // --- Update contributor list ---
        let mut funders = get_invoice_funders(&env, invoice_id);
        let mut found = false;
        for i in 0..funders.len() {
            let (addr, amt) = funders.get(i).unwrap();
            if addr == funder {
                funders.set(i, (addr, amt + fund_amount));
                found = true;
                break;
            }
        }
        if !found {
            funders.push_back((funder.clone(), fund_amount));
        }
        save_invoice_funders(&env, invoice_id, &funders);

        // --- Update invoice state ---
        invoice.amount_funded += fund_amount;

        if invoice.amount_funded == invoice.amount {
            // Fully funded — pay out to freelancer
            let discount_amount = invoice
                .amount
                .checked_mul(discount_rate_as_i128(invoice.discount_rate))
                .unwrap_or(0)
                / 10_000;
            let freelancer_payout = invoice.amount - discount_amount;

            token.transfer(&contract_address, &invoice.freelancer, &freelancer_payout);

            invoice.status = InvoiceStatus::Funded;
            invoice.funded_at = Some(env.ledger().timestamp());
            invoice.funder = Some(funder.clone());

            // Boost LP score on successful funding
            let current_lp_score = get_lp_score(&env, &funder);
            set_lp_score(&env, &funder, current_lp_score + 1);
        } else {
            invoice.status = InvoiceStatus::PartiallyFunded;
        }

        save_invoice(&env, &invoice);

        // Increment total funded counter if fully funded
        if invoice.status == InvoiceStatus::Funded {
            increment_total_funded(&env);
        }

        // Add to volume counter - get token list from storage
        let token_list: Vec<Address> = env
            .storage()
            .persistent()
            .get(&StorageKey::TokenList)
            .unwrap_or(Vec::new(&env));
        
        // Get token addresses from list, or use dummy addresses if not available
        let usdc_addr = if token_list.len() > 0 {
            token_list.get(0).unwrap()
        } else {
            invoice.token.clone()
        };
        let eurc_addr = if token_list.len() > 1 {
            token_list.get(1).unwrap()
        } else {
            invoice.token.clone()
        };
        let xlm_addr = if token_list.len() > 2 {
            token_list.get(2).unwrap()
        } else {
            invoice.token.clone()
        };
        add_volume(&env, &invoice.token, fund_amount, &usdc_addr, &eurc_addr, &xlm_addr);

        notify_distribution_funding(&env, &funder, fund_amount);

        env.events().publish_event(&InvoiceFunded {
            invoice_id: invoice.id,
            funder: funder.clone(),
            freelancer: invoice.freelancer.clone(),
            payer: invoice.payer.clone(),
            token: invoice.token.clone(),
            fund_amount,
            amount_funded: invoice.amount_funded,
            invoice_amount: invoice.amount,
            due_date: invoice.due_date,
            discount_rate: invoice.discount_rate,
            funded_at: invoice.funded_at,
            status: invoice.status.clone(),
        });

        Ok(())
    }

    // ------------------------------------------------------------
    // transfer_invoice
    // ------------------------------------------------------------
    pub fn transfer_invoice(
        env: Env,
        invoice_id: u64,
        new_freelancer: Address,
    ) -> Result<(), ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        invoice.freelancer.require_auth();

        match invoice.status {
            InvoiceStatus::Pending => {}
            InvoiceStatus::PartiallyFunded | InvoiceStatus::Funded => {
                return Err(ContractError::AlreadyFunded)
            }
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        let old_freelancer = invoice.freelancer.clone();
        invoice.freelancer = new_freelancer.clone();

        save_invoice(&env, &invoice);

        env.events().publish_event(&InvoiceTransferred {
            invoice_id,
            old_freelancer,
            new_freelancer,
            status: invoice.status.clone(),
        });

        Ok(())
    }

    // ------------------------------------------------------------
    // cancel_invoice
    // ------------------------------------------------------------
    pub fn cancel_invoice(env: Env, invoice_id: u64) -> Result<(), ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        invoice.freelancer.require_auth();

        match invoice.status {
            InvoiceStatus::Pending => {}
            InvoiceStatus::PartiallyFunded => {
                let funders = get_invoice_funders(&env, invoice_id);
                let token = token_client(&env, &invoice.token);
                let contract_address = env.current_contract_address();
                for i in 0..funders.len() {
                    let (funder_addr, fund_amt) = funders.get(i).unwrap();
                    let fund_discount = fund_amt
                        .checked_mul(discount_rate_as_i128(invoice.discount_rate))
                        .unwrap_or(0)
                        / 10_000;
                    let refund = fund_amt - fund_discount;
                    token.transfer(&contract_address, &funder_addr, &refund);
                }
            }
            InvoiceStatus::Funded => return Err(ContractError::AlreadyFunded),
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        invoice.status = InvoiceStatus::Cancelled;

        save_invoice(&env, &invoice);

        env.events().publish_event(&InvoiceCancelled {
            invoice_id,
            freelancer: invoice.freelancer.clone(),
            status: invoice.status.clone(),
        });

        Ok(())
    }

    // ------------------------------------------------------------
    // expire_invoice
    // ------------------------------------------------------------
    pub fn expire_invoice(env: Env, invoice_id: u64) -> Result<(), ContractError> {
        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        if env.ledger().timestamp() < invoice.due_date {
            return Err(ContractError::NotYetDefaulted);
        }

        match invoice.status {
            InvoiceStatus::Pending => {
                invoice.status = InvoiceStatus::Expired;
                save_invoice(&env, &invoice);
                Ok(())
            }
            InvoiceStatus::PartiallyFunded | InvoiceStatus::Funded => {
                Err(ContractError::AlreadyFunded)
            }
            InvoiceStatus::Paid => Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => Err(ContractError::AlreadyCancelled),
        }
    }

    // ------------------------------------------------------------
    // mark_paid (USES invoice.token)
    // ------------------------------------------------------------
    pub fn mark_paid(env: Env, invoice_id: u64) -> Result<(), ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        invoice.payer.require_auth();

        match invoice.status {
            InvoiceStatus::Pending | InvoiceStatus::PartiallyFunded => {
                return Err(ContractError::NotFunded)
            }
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Funded => {}
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        let funders = get_invoice_funders(&env, invoice_id);
        if funders.len() == 0 {
            return Err(ContractError::NotFunded);
        }

        let token = token_client(&env, &invoice.token);
        let contract_address = env.current_contract_address();

        // Payer sends full invoice amount to the contract
        token.transfer(&invoice.payer, &contract_address, &invoice.amount);

        // Calculate protocol fee and deduct it
        let fee_rate: u32 = env
            .storage()
            .instance()
            .get(&StorageKey::FeeRate)
            .unwrap_or(0);
        let protocol_fee = invoice
            .amount
            .checked_mul(fee_rate as i128)
            .unwrap_or(0)
            / 10_000;

        if protocol_fee > 0 {
            let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
            token.transfer(&contract_address, &admin, &protocol_fee);
        }

        let distribute_amount = invoice.amount - protocol_fee;

        // Distribute proportionally to funders
        for i in 0..funders.len() {
            let (funder_addr, fund_amt) = funders.get(i).unwrap();
            let funder_share = distribute_amount
                .checked_mul(fund_amt)
                .unwrap_or(0)
                / invoice.amount;
            if funder_share > 0 {
                token.transfer(&contract_address, &funder_addr, &funder_share);
            }
        }

        // ---- Update invoice ----
        invoice.status = InvoiceStatus::Paid;

        save_invoice(&env, &invoice);

        // Increment total paid counter
        increment_total_paid(&env);

        let paid_on_time = env.ledger().timestamp() <= invoice.due_date;
        notify_distribution_settlement(&env, &invoice.freelancer, &invoice.payer, paid_on_time);

        // --- Update payer reputation ---
        let current_score = get_payer_score(&env, &invoice.payer);
        set_payer_score(&env, &invoice.payer, current_score + 1);

        env.events().publish_event(&InvoicePaid {
            invoice_id: invoice.id,
            payer: invoice.payer.clone(),
            funder: funders.get(0).unwrap().0.clone(),
            freelancer: invoice.freelancer.clone(),
            token: invoice.token.clone(),
            amount: invoice.amount,
            discount_amount: invoice.amount - distribute_amount,
            due_date: invoice.due_date,
            paid_on_time,
            status: invoice.status.clone(),
        });

        Ok(())
    }

    // ----------------------------------------------------------------
    // claim_yield
    // ----------------------------------------------------------------
    pub fn claim_yield(env: Env, invoice_id: u64) -> Result<i128, ContractError> {
        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let invoice = load_invoice(&env, invoice_id);

        // Only the funder can query their own yield
        if let Some(ref funder) = invoice.funder {
            funder.require_auth();
        } else {
            return Err(ContractError::NothingToClaim);
        }

        match invoice.status {
            InvoiceStatus::Pending
            | InvoiceStatus::PartiallyFunded
            | InvoiceStatus::Funded => Ok(0),
            InvoiceStatus::Defaulted => Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => Err(ContractError::AlreadyCancelled),
            InvoiceStatus::Paid => {
                let yield_amount = invoice
                    .amount
                    .checked_mul(discount_rate_as_i128(invoice.discount_rate))
                    .unwrap_or(0)
                    / 10_000;
                Ok(yield_amount)
            }
        }
    }

    // ----------------------------------------------------------------
    // claim_default
    // ----------------------------------------------------------------
    pub fn claim_default(env: Env, funder: Address, invoice_id: u64) -> Result<(), ContractError> {
        if is_paused(&env) {
            return Err(ContractError::ContractPaused);
        }

        funder.require_auth();

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        let funders = get_invoice_funders(&env, invoice_id);
        let mut is_funder = false;
        for i in 0..funders.len() {
            if funders.get(i).unwrap().0 == funder {
                is_funder = true;
                break;
            }
        }

        if !is_funder {
            return Err(ContractError::Unauthorized);
        }

        let now = env.ledger().timestamp();
        if now < invoice.due_date {
            return Err(ContractError::NotYetDefaulted);
        }

        match invoice.status {
            InvoiceStatus::Funded => {}
            InvoiceStatus::Pending | InvoiceStatus::PartiallyFunded => {
                return Err(ContractError::NotFunded)
            }
            InvoiceStatus::Paid => return Err(ContractError::AlreadyPaid),
            InvoiceStatus::Defaulted => return Err(ContractError::InvoiceDefaulted),
            InvoiceStatus::Appealed => return Err(ContractError::InvoiceAppealed),
            InvoiceStatus::Expired => return Err(ContractError::InvoiceExpired),
            InvoiceStatus::Cancelled => return Err(ContractError::AlreadyCancelled),
        }

        let token = token_client(&env, &invoice.token);
        let contract_address = env.current_contract_address();

        let mut total_refunded = 0;

        for i in 0..funders.len() {
            let (funder_addr, fund_amt) = funders.get(i).unwrap();
            let fund_discount = fund_amt
                .checked_mul(discount_rate_as_i128(invoice.discount_rate))
                .unwrap_or(0)
                / 10_000;
            let refund = fund_amt - fund_discount;
            token.transfer(&contract_address, &funder_addr, &refund);
            total_refunded += refund;
        }

        invoice.status = InvoiceStatus::Defaulted;
        save_invoice(&env, &invoice);

        // --- Update payer reputation ---
        // Snapshot the score BEFORE applying the penalty so appeal_default()
        // can restore it exactly if the appeal is upheld.
        let current_score = get_payer_score(&env, &invoice.payer);
        save_pre_default_payer_score(&env, invoice_id, current_score);

        if current_score > 5 {
            set_payer_score(&env, &invoice.payer, current_score - 5);
        } else {
            set_payer_score(&env, &invoice.payer, 0);
        }

        env.events().publish_event(&InvoiceDefaulted {
            invoice_id: invoice.id,
            funder,
            freelancer: invoice.freelancer.clone(),
            payer: invoice.payer.clone(),
            token: invoice.token.clone(),
            amount: invoice.amount,
            due_date: invoice.due_date,
            defaulted_at: now,
            discount_amount: total_refunded,
            status: invoice.status.clone(),
        });

        Ok(())
    }

    // ================================================================
    // Issue #36: appeal_default — payer contests an unfair default
    //
    // Flow:
    //   1. Payer calls `appeal_default(invoice_id, evidence_hash)`.
    //   2. Invoice transitions to `Appealed` status.
    //   3. Admin/governance calls `resolve_appeal(invoice_id, upheld)`.
    //      - upheld=true  → default reversed, score restored.
    //      - upheld=false → invoice remains Defaulted.
    // ================================================================

    /// File an appeal against an unfair default marking.
    ///
    /// * `invoice_id`    – the defaulted invoice
    /// * `evidence_hash` – SHA-256 hash of off-chain evidence provided by the payer
    pub fn appeal_default(
        env: Env,
        invoice_id: u64,
        evidence_hash: BytesN<32>,
    ) -> Result<(), ContractError> {
        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        // Only the payer may appeal.
        invoice.payer.require_auth();

        // Check AlreadyAppealed BEFORE status check: after the first appeal the
        // status is `Appealed` (not `Defaulted`), so the status guard would fire
        // with the wrong error code if checked first.
        if get_appeal(&env, invoice_id).is_some() {
            return Err(ContractError::AlreadyAppealed);
        }

        // Invoice must be in Defaulted state.
        if invoice.status != InvoiceStatus::Defaulted {
            return Err(ContractError::NotDefaulted);
        }

        let now = env.ledger().timestamp();

        // Appeal must be filed within the appeal window after default.
        // A default can only occur after due_date, so we measure from due_date.
        if now > invoice.due_date + APPEAL_WINDOW_SECONDS {
            return Err(ContractError::AppealWindowClosed);
        }

        // Use the pre-default score snapshot saved by claim_default().
        // Fall back to the current score if somehow missing (shouldn't happen).
        let pre_default_score = get_pre_default_payer_score(&env, invoice_id)
            .unwrap_or_else(|| get_payer_score(&env, &invoice.payer));

        save_appeal(
            &env,
            invoice_id,
            &AppealRecord {
                evidence_hash: evidence_hash.clone(),
                appealed_at: now,
                pre_default_score,
            },
        );

        invoice.status = InvoiceStatus::Appealed;
        save_invoice(&env, &invoice);

        env.events().publish_event(&DefaultAppealed {
            invoice_id,
            payer: invoice.payer.clone(),
            evidence_hash,
            appealed_at: now,
        });

        Ok(())
    }

    /// Resolve a pending appeal (admin / governance only).
    ///
    /// * `upheld=true`  → reverse the default, restore pre-default score, status → Defaulted (reversed).
    ///   In practice the status transitions back to Defaulted with score restored so the LP
    ///   can still collect principal they were already refunded. The key effect is reputation repair.
    /// * `upheld=false` → reject the appeal; invoice remains Defaulted (status reverts from Appealed).
    pub fn resolve_appeal(
        env: Env,
        invoice_id: u64,
        upheld: bool,
    ) -> Result<(), ContractError> {
        let admin: Address = env.storage().instance().get(&StorageKey::Admin).unwrap();
        admin.require_auth();

        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }

        let mut invoice = load_invoice(&env, invoice_id);

        if invoice.status != InvoiceStatus::Appealed {
            return Err(ContractError::NotDefaulted);
        }

        let appeal = get_appeal(&env, invoice_id).ok_or(ContractError::InvoiceNotFound)?;

        let now = env.ledger().timestamp();

        if upheld {
            // Restore the payer's reputation to what it was before the default.
            set_payer_score(&env, &invoice.payer, appeal.pre_default_score);
            // Status moves back to Defaulted — the LP still received their refund,
            // but the reputational penalty on the payer is reversed.
            invoice.status = InvoiceStatus::Defaulted;
        } else {
            // Appeal rejected; mark as Defaulted again (was temporarily Appealed).
            invoice.status = InvoiceStatus::Defaulted;
        }

        save_invoice(&env, &invoice);

        env.events().publish_event(&AppealResolved {
            invoice_id,
            payer: invoice.payer.clone(),
            upheld,
            resolved_at: now,
        });

        Ok(())
    }

    // ----------------------------------------------------------------
    // payer_score
    // ----------------------------------------------------------------
    pub fn payer_score(env: Env, payer: Address) -> u32 {
        get_payer_score(&env, &payer)
    }

    // ----------------------------------------------------------------
    // lp_score  (Issue #34)
    // ----------------------------------------------------------------
    pub fn lp_score(env: Env, lp: Address) -> u32 {
        get_lp_score(&env, &lp)
    }

    // ----------------------------------------------------------------
    // suggested_discount_rate
    // ----------------------------------------------------------------
    pub fn suggested_discount_rate(env: Env, payer: Address) -> u32 {
        let score = get_payer_score(&env, &payer);
        let capped = score.min(100);
        let rate = 500 + (100 - capped) * 5;
        rate.max(50)
    }

    // ----------------------------------------------------------------
    // get_invoice
    // ----------------------------------------------------------------
    pub fn get_invoice(env: Env, invoice_id: u64) -> Result<Invoice, ContractError> {
        if !invoice_exists(&env, invoice_id) {
            return Err(ContractError::InvoiceNotFound);
        }
        Ok(load_invoice(&env, invoice_id))
    }

    pub fn get_invoice_count(env: Env) -> u64 {
        env.storage()
            .persistent()
            .get(&StorageKey::InvoiceCount)
            .unwrap_or(0)
    }
}

// ----------------------------------------------------------------
// TOKEN HELPERS
// ----------------------------------------------------------------

fn token_client<'a>(env: &'a Env, token: &Address) -> TokenClient<'a> {
    TokenClient::new(env, token)
}

fn discount_rate_as_i128(rate: u32) -> i128 {
    rate as i128
}

fn validate_invoice_terms(
    env: &Env,
    amount: i128,
    due_date: u64,
    discount_rate: u32,
) -> Result<(), ContractError> {
    if amount < 1_000_000 {
        return Err(ContractError::InvalidAmount);
    }

    let max_rate: u32 = env
        .storage()
        .instance()
        .get(&StorageKey::MaxDiscountRate)
        .unwrap_or(5000);
    if discount_rate == 0 || discount_rate > max_rate {
        return Err(ContractError::InvalidDiscountRate);
    }

    let now = env.ledger().timestamp();
    
    // Validate due date is in the future
    if due_date <= now {
        return Err(ContractError::InvalidDueDate);
    }

    let max_offset = 365 * 24 * 60 * 60;
    if due_date > now + max_offset {
        return Err(ContractError::InvalidDueDate);
    }

    Ok(())
}

fn is_approved_token(env: &Env, token: &Address) -> bool {
    env.storage()
        .persistent()
        .get(&StorageKey::ApprovedToken(token.clone()))
        .unwrap_or(false)
}

fn notify_distribution_funding(env: &Env, lp: &Address, amount_usdc_equivalent: i128) {
    let Some(dist_contract) = env
        .storage()
        .instance()
        .get::<_, Address>(&StorageKey::DistributionContract)
    else {
        return;
    };

    let args = vec![
        env,
        lp.clone().into_val(env),
        amount_usdc_equivalent.into_val(env),
    ];
    env.invoke_contract::<()>(&dist_contract, &Symbol::new(env, "accrue_lp"), args);
}

fn notify_distribution_settlement(
    env: &Env,
    freelancer: &Address,
    payer: &Address,
    settled_on_time: bool,
) {
    let Some(dist_contract) = env
        .storage()
        .instance()
        .get::<_, Address>(&StorageKey::DistributionContract)
    else {
        return;
    };

    let args = vec![
        env,
        freelancer.clone().into_val(env),
        payer.clone().into_val(env),
        settled_on_time.into_val(env),
    ];
    env.invoke_contract::<()>(&dist_contract, &Symbol::new(env, "accrue_settlement"), args);
}

// ----------------------------------------------------------------
// TEST MODULES
// ----------------------------------------------------------------

mod test;
mod tests_appeal;
mod tests_arithmetic;
mod tests_auth;
mod tests_distribution;
mod tests_invariants;
mod tests_lp_priority_queue;
mod tests_mutation;
mod tests_protocol_fee;
mod tests_security;
mod tests_state_machine;
mod tests_storage;
