// Event emission tests to catch regressions where events are dropped.
// This file is placed per `fix.md` to provide a consolidated smoke-test
// that exercises common instructions and asserts events are emitted.

#![cfg(test)]

use soroban_sdk::{testutils::Address as _, Address, Env};

// Lightweight smoke tests for event emission. These mirror patterns used in
// the individual contract test suites and assert that calling key
// instructions results in an event being published for the contract.

#[test]
fn invoice_liquidity_submit_emits_event() {
    use invoice_liquidity::InvoiceLiquidityContractClient;
    use soroban_sdk::token::StellarAssetClient;

    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let usdc_admin = Address::generate(&env);
    let usdc_id = env.register_stellar_asset_contract_v2(usdc_admin);
    let usdc = usdc_id.address();

    let xlm_admin = Address::generate(&env);
    let xlm_id = env.register_stellar_asset_contract_v2(xlm_admin);
    let xlm = xlm_id.address();

    let contract_id = env.register(invoice_liquidity::InvoiceLiquidityContract, ());
    let client = InvoiceLiquidityContractClient::new(&env, &contract_id);

    client.initialize(&admin, &usdc, &xlm);

    let freelancer = Address::generate(&env);
    let payer = Address::generate(&env);
    let token_client = StellarAssetClient::new(&env, &usdc);
    token_client.mint(&payer, &1_000_000i128);

    let due_date = env.ledger().timestamp() + 1000u64;
    let id = client.submit_invoice(&freelancer, &payer, &1_000_000i128, &due_date, &100u32, &usdc);

    let events = env.events().all().filter_by_contract(&client.address);
    assert!(events.events().last().is_some(), "submit_invoice must emit an event");

    // Basic sanity: ensure the last event contains the invoice id as bytes.
    let last = events.events().last().unwrap();
    let s = format!("{:?}", last);
    assert!(s.contains(&format!("{}", id)), "event must reference invoice id");
}
