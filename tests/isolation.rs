#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Tenant isolation suite (PRP §2.3, §12). Its own test binary so it can be
//! run alone: `cargo test --test isolation`.
//!
//! The compile-level half of the invariant is that no repository method
//! accepts an optional tenant — every signature takes `TenantId`. These tests
//! cover the runtime half: rows written for one tenant are unreachable from
//! any other.

use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use veille::domain::{Account, Holding, Transaction};
use veille::store::Store;

fn account(external_id: &str, name: &str) -> Account {
    Account {
        external_id: external_id.into(),
        name: name.into(),
        institution: Some("Test Bank".into()),
        kind: "depository".into(),
        status: "active".into(),
        balance_minor: 100_000,
        currency: "USD".into(),
    }
}

fn transaction(external_id: &str, description: &str) -> Transaction {
    Transaction {
        external_id: external_id.into(),
        account_external_id: "acct-1".into(),
        posted_at: "2026-08-01".parse().expect("valid date"),
        amount_minor: -4_200,
        currency: "USD".into(),
        description: description.into(),
        category: None,
        counterparty_key: Some(description.to_lowercase()),
        is_transfer: false,
        updated_at: Utc
            .with_ymd_and_hms(2026, 8, 1, 12, 0, 0)
            .single()
            .expect("valid ts"),
    }
}

fn holding(symbol: &str) -> Holding {
    Holding {
        account_external_id: "acct-1".into(),
        symbol: symbol.into(),
        quantity: "2.5".into(),
        market_value_minor: Some(50_000),
        currency: "USD".into(),
    }
}

async fn open_store(dir: &TempDir) -> Store {
    Store::open(&dir.path().join("test.sqlite3"))
        .await
        .expect("store opens")
}

#[tokio::test]
async fn rows_written_for_one_tenant_are_invisible_to_another() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;
    let now = Utc::now();

    let a = store
        .tenants()
        .ensure("alpha", "Alpha")
        .await
        .expect("tenant a");
    let b = store
        .tenants()
        .ensure("beta", "Beta")
        .await
        .expect("tenant b");

    store
        .insert_account_snapshots(a, &[account("acct-1", "A Checking")], now)
        .await
        .expect("snapshot a");
    store
        .upsert_transactions(a, &[transaction("txn-1", "Coffee")], now)
        .await
        .expect("txns a");
    store
        .insert_holding_snapshots(a, &[holding("VTI")], now)
        .await
        .expect("holdings a");

    assert_eq!(store.transaction_count(b).await.expect("count b"), 0);
    assert_eq!(
        store.account_snapshot_count(b).await.expect("snap count b"),
        0
    );
    assert_eq!(
        store.holding_snapshot_count(b).await.expect("hold count b"),
        0
    );
    assert!(store.list_transactions(b).await.expect("list b").is_empty());

    assert_eq!(store.transaction_count(a).await.expect("count a"), 1);
    assert_eq!(
        store.account_snapshot_count(a).await.expect("snap count a"),
        1
    );
    assert_eq!(
        store.holding_snapshot_count(a).await.expect("hold count a"),
        1
    );
}

#[tokio::test]
async fn same_external_id_may_exist_in_two_tenants_without_collision() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;
    let now = Utc::now();

    let a = store
        .tenants()
        .ensure("alpha", "Alpha")
        .await
        .expect("tenant a");
    let b = store
        .tenants()
        .ensure("beta", "Beta")
        .await
        .expect("tenant b");

    // Same upstream UUID landing in two tenants must not collide — uniqueness
    // is per (tenant_id, external_id), not global.
    store
        .upsert_transactions(a, &[transaction("shared-id", "A's payment")], now)
        .await
        .expect("txn a");
    store
        .upsert_transactions(b, &[transaction("shared-id", "B's payment")], now)
        .await
        .expect("txn b");

    let a_rows = store.list_transactions(a).await.expect("list a");
    let b_rows = store.list_transactions(b).await.expect("list b");
    assert_eq!(a_rows.len(), 1);
    assert_eq!(b_rows.len(), 1);
    assert_eq!(a_rows[0].description, "A's payment");
    assert_eq!(b_rows[0].description, "B's payment");
}

#[tokio::test]
async fn tenant_ensure_is_idempotent_and_lookup_matches() {
    let dir = TempDir::new().expect("tempdir");
    let store = open_store(&dir).await;

    let first = store
        .tenants()
        .ensure("alpha", "Alpha")
        .await
        .expect("ensure 1");
    let second = store
        .tenants()
        .ensure("alpha", "Alpha")
        .await
        .expect("ensure 2");
    assert_eq!(first, second);

    let looked_up = store.tenants().by_slug("alpha").await.expect("lookup");
    assert_eq!(looked_up, first);

    let missing = store.tenants().by_slug("nope").await;
    assert!(
        missing.is_err(),
        "unknown slug must be an error, not a default"
    );
}
