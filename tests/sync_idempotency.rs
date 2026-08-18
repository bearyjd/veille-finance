#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Phase 1 gate (PRP §11): sync twice against a fixture, assert zero
//! duplicate rows.

use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use veille::source::fixture::FixtureSureSource;
use veille::store::Store;
use veille::sync::{SyncOptions, sync_tenant};

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
    let outcome1 = sync_tenant(&store, tenant, &source, first_run, SyncOptions::default())
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
    let outcome2 = sync_tenant(&store, tenant, &source, second_run, SyncOptions::default())
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
    let outcome = sync_tenant(&store, tenant, &source, now, SyncOptions::default())
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

#[tokio::test]
async fn stale_duplicate_in_a_batch_cannot_overwrite_newer_data() {
    use chrono::TimeZone;
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");

    let newer = test_txn(
        "t1",
        "corrected",
        Utc.with_ymd_and_hms(2026, 8, 18, 0, 0, 0),
    );
    let stale = test_txn("t1", "old", Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0));
    let now = Utc
        .with_ymd_and_hms(2026, 8, 18, 12, 0, 0)
        .single()
        .expect("ts");

    store
        .upsert_transactions(tenant, &[newer.clone(), stale.clone()], now)
        .await
        .expect("batch");
    let rows = store.list_transactions(tenant).await.expect("list");
    assert_eq!(
        rows[0].description, "corrected",
        "stale row in batch must not win"
    );

    // A later run carrying only the stale version must also be a no-op.
    store
        .upsert_transactions(tenant, &[stale], now)
        .await
        .expect("stale rerun");
    let rows = store.list_transactions(tenant).await.expect("list");
    assert_eq!(rows[0].description, "corrected");
}

#[tokio::test]
async fn pathological_stored_date_does_not_panic_the_watermark() {
    use chrono::TimeZone;
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));

    let mut weird = test_txn(
        "t-min",
        "min date",
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0),
    );
    weird.posted_at = chrono::NaiveDate::MIN;
    let now = Utc
        .with_ymd_and_hms(2026, 8, 18, 12, 0, 0)
        .single()
        .expect("ts");
    store
        .upsert_transactions(tenant, &[weird], now)
        .await
        .expect("weird row");

    // Must not panic; overflowing watermark arithmetic degrades to a full fetch.
    sync_tenant(&store, tenant, &source, now, SyncOptions::default())
        .await
        .expect("sync survives");
}

#[tokio::test]
async fn lookback_days_bounds_the_incremental_window() {
    use chrono::TimeZone;
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));

    // Seed the watermark at 2026-08-01.
    let seed = test_txn("seed", "seed", Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0));
    let now = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    store
        .upsert_transactions(tenant, &[seed], now)
        .await
        .expect("seed");

    // Lookback 7 days => since 2026-07-25: only txn-0001 (2026-08-01) qualifies.
    let outcome = sync_tenant(
        &store,
        tenant,
        &source,
        now,
        SyncOptions {
            lookback_days: 7,
            full: false,
        },
    )
    .await
    .expect("sync");
    assert_eq!(outcome.transactions.inserted, 1);
    assert_eq!(store.transaction_count(tenant).await.expect("count"), 2);

    // full: true ignores the watermark entirely.
    let outcome = sync_tenant(
        &store,
        tenant,
        &source,
        now,
        SyncOptions {
            lookback_days: 7,
            full: true,
        },
    )
    .await
    .expect("full sync");
    assert_eq!(
        outcome.transactions.inserted, 2,
        "full resync picks up the older rows"
    );
}

fn test_txn(
    external_id: &str,
    description: &str,
    updated_at: chrono::LocalResult<chrono::DateTime<Utc>>,
) -> veille::domain::Transaction {
    veille::domain::Transaction {
        external_id: external_id.into(),
        account_external_id: "acct-1".into(),
        posted_at: "2026-08-01".parse().expect("date"),
        amount_minor: -100,
        currency: "USD".into(),
        description: description.into(),
        category: None,
        counterparty_key: None,
        is_transfer: false,
        updated_at: updated_at.single().expect("ts"),
    }
}

#[tokio::test]
async fn future_dated_transactions_do_not_poison_the_watermark() {
    use chrono::TimeZone;
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));

    // A post-dated row (e.g. scheduled payment) far in the future.
    let mut future = test_txn(
        "future",
        "post-dated",
        Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0),
    );
    future.posted_at = "2026-09-05".parse().expect("date");
    let now = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    store
        .upsert_transactions(tenant, &[future], now)
        .await
        .expect("seed");

    // Unclamped, since would be 2026-09-05 - 30d = 2026-08-06 and txn-0001
    // (2026-08-01) would be invisible. Clamped to now, since = 2026-07-18.
    let outcome = sync_tenant(
        &store,
        tenant,
        &source,
        now,
        SyncOptions {
            lookback_days: 30,
            full: false,
        },
    )
    .await
    .expect("sync");
    assert_eq!(
        outcome.transactions.inserted, 1,
        "the watermark must anchor to min(newest, now)"
    );
}

#[tokio::test]
async fn sync_persists_upstream_health_replacing_prior_rows() {
    use chrono::TimeZone;
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));

    let first = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    sync_tenant(&store, tenant, &source, first, SyncOptions::default())
        .await
        .expect("sync 1");

    let rows = store.upstream_health(tenant).await.expect("health rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].institution, "First National");
    assert_eq!(
        rows[0].last_successful_sync_at,
        Utc.with_ymd_and_hms(2026, 8, 15, 6, 0, 5).single()
    );

    // Second sync replaces, never accumulates.
    let second = Utc
        .with_ymd_and_hms(2026, 8, 18, 22, 0, 0)
        .single()
        .expect("ts");
    sync_tenant(&store, tenant, &source, second, SyncOptions::default())
        .await
        .expect("sync 2");
    let rows = store.upstream_health(tenant).await.expect("health rows");
    assert_eq!(rows.len(), 1, "health is current state, not history");

    // And it is tenant-scoped like everything else.
    let other = store.tenants().ensure("b", "B").await.expect("tenant b");
    assert!(
        store
            .upstream_health(other)
            .await
            .expect("empty")
            .is_empty()
    );
}
