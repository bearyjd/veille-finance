#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Phase 1 gate (PRP §11): sync twice against a fixture, assert zero
//! duplicate rows.

use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use veille::source::fixture::FixtureSureSource;
use veille::store::Store;
use veille::sync::sync_tenant;

#[tokio::test]
async fn double_sync_creates_no_duplicate_transactions() {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("test.sqlite3"))
        .await
        .expect("store");
    let tenant = store
        .tenants()
        .ensure("alpha", "Alpha")
        .await
        .expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));

    let first_run = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    let outcome1 = sync_tenant(&store, tenant, &source, first_run)
        .await
        .expect("sync 1");
    assert_eq!(outcome1.transactions.inserted, 3);
    assert_eq!(outcome1.transactions.refreshed, 0);
    assert_eq!(outcome1.account_snapshots, 2);
    assert_eq!(outcome1.holding_snapshots, 1);

    let second_run = Utc
        .with_ymd_and_hms(2026, 8, 18, 22, 0, 0)
        .single()
        .expect("ts");
    let outcome2 = sync_tenant(&store, tenant, &source, second_run)
        .await
        .expect("sync 2");
    assert_eq!(
        outcome2.transactions.inserted, 0,
        "second sync must insert nothing new"
    );

    // Transactions: idempotent — count unchanged.
    assert_eq!(store.transaction_count(tenant).await.expect("count"), 3);
    // Snapshots: append-only by design — exactly one new row per entity per run.
    assert_eq!(store.account_snapshot_count(tenant).await.expect("snap"), 4);
    assert_eq!(store.holding_snapshot_count(tenant).await.expect("hold"), 2);
}

#[tokio::test]
async fn full_backfill_when_store_is_empty_incremental_afterwards() {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("test.sqlite3"))
        .await
        .expect("store");
    let tenant = store
        .tenants()
        .ensure("alpha", "Alpha")
        .await
        .expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));

    let now = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    let outcome = sync_tenant(&store, tenant, &source, now)
        .await
        .expect("sync");
    // Empty store → full history pulled, including the oldest fixture
    // transaction from 2026-07-01.
    assert_eq!(outcome.transactions.inserted, 3);

    let rows = store.list_transactions(tenant).await.expect("list");
    assert!(rows.iter().any(|t| t.external_id == "txn-0003"));
    // Health came along for the ride (feeds sync-stale in Phase 2).
    assert_eq!(outcome.health.institutions.len(), 1);
}
