#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Contract tests: the wire module must parse the golden captures taken from
//! a live `sure:stable` instance in Phase 0, and convert them into domain
//! values with the documented semantics.

use veille::domain::{Account, Holding, Transaction};
use veille::source::wire::{AccountsPage, HoldingsPage, SyncsPage, TransactionsPage};

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/fixtures/phase0/rest/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

#[test]
fn parses_accounts_page_and_converts() {
    let page: AccountsPage = serde_json::from_str(&fixture("accounts")).expect("parse accounts");
    assert_eq!(page.pagination.total_count, 20);
    assert_eq!(page.accounts.len(), 20);

    let honda = page
        .accounts
        .into_iter()
        .find(|a| a.name == "2016 Honda Accord")
        .expect("demo account present");
    let domain: Account = honda.into();
    assert_eq!(domain.external_id, "af931d31-48f5-4c2c-aa79-7aab6c39af05");
    assert_eq!(domain.balance_minor, 1_800_000);
    assert_eq!(domain.currency, "USD");
    assert_eq!(domain.kind, "vehicle");
    assert_eq!(domain.status, "active");
    assert_eq!(domain.institution, None);
}

#[test]
fn parses_transactions_page_and_converts() {
    let page: TransactionsPage =
        serde_json::from_str(&fixture("transactions")).expect("parse transactions");
    assert_eq!(page.pagination.total_count, 7544);

    let wire = page
        .transactions
        .into_iter()
        .find(|t| t.id == "09746507-7a03-4dab-a253-68dcbb4a9cde")
        .expect("known transaction present");
    let domain: Transaction = wire.into();

    // signed_amount_cents is positive-for-income; domain keeps that convention.
    assert_eq!(domain.amount_minor, 82_700);
    assert_eq!(domain.posted_at.to_string(), "2026-09-05");
    assert_eq!(
        domain.account_external_id,
        "5bda6c0b-d1c8-4118-8cf6-04938de123ca"
    );
    assert_eq!(domain.description, "Sapphire Payment");
    assert_eq!(domain.category, None);
    assert!(domain.is_transfer);
    // No merchant on this transaction: counterparty falls back to the
    // normalized description.
    assert_eq!(domain.counterparty_key.as_deref(), Some("sapphire payment"));
    // Sure's own UUID is the stable external id (upstream external_id is null
    // for manual entries).
    assert_eq!(domain.external_id, "09746507-7a03-4dab-a253-68dcbb4a9cde");
}

#[test]
fn parses_holdings_page_and_converts() {
    let page: HoldingsPage = serde_json::from_str(&fixture("holdings")).expect("parse holdings");
    let vxus = page
        .holdings
        .into_iter()
        .find(|h| h.security.ticker.as_deref() == Some("VXUS"))
        .expect("VXUS holding present");
    let domain: Holding = vxus.into();
    assert_eq!(domain.symbol, "VXUS");
    assert_eq!(domain.quantity, "3.21");
    assert_eq!(domain.market_value_minor, Some(43_014));
    assert_eq!(domain.currency, "USD");
    assert_eq!(
        domain.account_external_id,
        "62fe6cc7-058e-4932-b748-f0604a69ff48"
    );
}

#[test]
fn parses_syncs_page() {
    let page: SyncsPage = serde_json::from_str(&fixture("syncs")).expect("parse syncs");
    assert_eq!(page.meta.total_count, 20);
    let first = &page.data[0];
    assert_eq!(first.status, "completed");
    assert!(first.completed_at.is_some());
    let syncable = first.syncable.as_ref().expect("syncable present");
    assert_eq!(syncable.kind, "Account");
}

#[test]
fn formatted_money_parser_is_strict() {
    use veille::source::wire::parse_formatted_money;

    assert_eq!(parse_formatted_money("$430.14", "USD"), Some(43_014));
    assert_eq!(parse_formatted_money("$18,000.00", "USD"), Some(1_800_000));
    assert_eq!(parse_formatted_money("-$827.00", "USD"), Some(-82_700));
    assert_eq!(parse_formatted_money("£1,234.56", "GBP"), Some(123_456));
    // Zero-decimal currency
    assert_eq!(parse_formatted_money("¥1,500", "JPY"), Some(1_500));

    // Anything surprising must refuse, never guess:
    assert_eq!(parse_formatted_money("430.14", "USD"), None); // missing symbol
    assert_eq!(parse_formatted_money("$430.1", "USD"), None); // wrong exponent
    assert_eq!(parse_formatted_money("$4,30.14", "USD"), None); // bad grouping
    assert_eq!(parse_formatted_money("€1.234,56", "EUR"), None); // unsupported locale layout
    assert_eq!(parse_formatted_money("$430.14 CR", "USD"), None); // trailing junk
    assert_eq!(parse_formatted_money("", "USD"), None);
}
