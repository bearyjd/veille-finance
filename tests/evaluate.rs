#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Engine-level Phase 2 gate: evaluation over a store synced from fixtures,
//! sync-stale firing on stale health, dry-run inertness, dedupe on
//! persistence, and deterministic output.

use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use veille::config::RuleThresholds;
use veille::domain::Severity;
use veille::rules::evaluate_tenant;
use veille::source::fixture::FixtureSureSource;
use veille::store::{Store, TenantId};
use veille::sync::{SyncOptions, sync_tenant};

async fn synced_store() -> (TempDir, Store, TenantId) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));
    let sync_now = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    sync_tenant(&store, tenant, &source, sync_now, SyncOptions::default())
        .await
        .expect("sync");
    (dir, store, tenant)
}

#[tokio::test]
async fn sync_stale_fires_when_upstream_health_is_stale() {
    let (_dir, store, tenant) = synced_store().await;

    // Fixture health: last success 2026-08-15T06:00:05Z. At Aug 20 that is
    // 5 days stale — past the default 4-day threshold.
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let findings = evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, false)
        .await
        .expect("evaluate");
    assert_eq!(
        findings.len(),
        1,
        "exactly the sync-stale finding: {findings:?}"
    );
    assert_eq!(findings[0].rule_id, "sync-stale");
    assert_eq!(findings[0].severity, Severity::Alert);

    // One day after the last success, nothing is stale.
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 16, 22, 0, 0)
        .single()
        .expect("ts");
    let findings = evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, false)
        .await
        .expect("evaluate");
    assert!(
        findings.iter().all(|f| f.rule_id != "sync-stale"),
        "fresh health must not fire sync-stale"
    );
}

#[tokio::test]
async fn dry_run_is_genuinely_inert() {
    let (_dir, store, tenant) = synced_store().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");

    let findings = evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, false)
        .await
        .expect("evaluate");
    assert!(
        !findings.is_empty(),
        "the scenario must produce findings to prove inertness"
    );
    assert_eq!(
        store.finding_count(tenant).await.expect("count"),
        0,
        "dry-run must write no findings"
    );
    assert_eq!(
        store.delivery_count(tenant).await.expect("count"),
        0,
        "dry-run must write no deliveries"
    );
}

#[tokio::test]
async fn persisted_findings_dedupe_across_runs() {
    let (_dir, store, tenant) = synced_store().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");

    let first = evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, true)
        .await
        .expect("evaluate 1");
    assert_eq!(
        store.finding_count(tenant).await.expect("count"),
        first.len() as u64
    );

    // Same condition re-detected later the same day: no new rows.
    let later = Utc
        .with_ymd_and_hms(2026, 8, 20, 23, 0, 0)
        .single()
        .expect("ts");
    evaluate_tenant(&store, tenant, RuleThresholds::default(), later, true)
        .await
        .expect("evaluate 2");
    assert_eq!(
        store.finding_count(tenant).await.expect("count"),
        first.len() as u64,
        "the same condition must not re-alert"
    );
}

#[tokio::test]
async fn evaluation_is_deterministic() {
    let (_dir, store, tenant) = synced_store().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");

    let a = evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, false)
        .await
        .expect("run a");
    let b = evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, false)
        .await
        .expect("run b");
    assert_eq!(
        a, b,
        "same store, same as-of, same findings — byte for byte"
    );
}

#[tokio::test]
async fn data_posted_after_as_of_is_invisible_to_evaluation() {
    let (_dir, store, tenant) = synced_store().await;

    // A future-dated transaction big enough to trip an enabled threshold.
    let future = veille::domain::Transaction {
        external_id: "future-wire".into(),
        account_external_id: "acct-linked-1".into(),
        posted_at: "2026-09-05".parse().expect("date"),
        amount_minor: -900_000,
        currency: "USD".into(),
        description: "Post-dated wire".into(),
        category: None,
        counterparty_key: Some("post-dated wire".into()),
        is_transfer: false,
        updated_at: Utc
            .with_ymd_and_hms(2026, 8, 17, 0, 0, 0)
            .single()
            .expect("ts"),
    };
    let now = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    store
        .upsert_transactions(tenant, &[future], now)
        .await
        .expect("seed");

    let thresholds = RuleThresholds {
        large_transfer_minor: Some(500_000),
        ..RuleThresholds::default()
    };

    // Evaluated at Aug 18, the Sep 5 row does not exist yet.
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 18, 22, 0, 0)
        .single()
        .expect("ts");
    let findings = evaluate_tenant(&store, tenant, thresholds.clone(), as_of, false)
        .await
        .expect("evaluate");
    assert!(
        findings.iter().all(|f| f.rule_id != "large-transfer"),
        "a transaction posted after --as-of must be invisible: {findings:?}"
    );

    // Evaluated after it posts, it fires.
    let as_of = Utc
        .with_ymd_and_hms(2026, 9, 6, 22, 0, 0)
        .single()
        .expect("ts");
    let findings = evaluate_tenant(&store, tenant, thresholds, as_of, false)
        .await
        .expect("evaluate");
    assert!(
        findings
            .iter()
            .any(|f| f.dedupe_key == "large-transfer:future-wire"),
        "the same transaction fires once its posted date has passed"
    );
}
