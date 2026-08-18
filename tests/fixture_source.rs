#![allow(clippy::expect_used, clippy::unwrap_used)]

//! FixtureSureSource must honor the SureSource contract: same wire parsing as
//! the real adapter, `since` filtering, and health derivation.

use chrono::{TimeZone, Utc};
use veille::source::SureSource;
use veille::source::fixture::FixtureSureSource;

fn synthetic() -> FixtureSureSource {
    FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")))
}

fn phase0() -> FixtureSureSource {
    FixtureSureSource::new(format!(
        "{}/fixtures/phase0/rest",
        env!("CARGO_MANIFEST_DIR")
    ))
}

#[tokio::test]
async fn loads_phase0_captures() {
    let src = phase0();
    let accounts = src.accounts().await.expect("accounts");
    assert_eq!(accounts.len(), 20);
    let transactions = src
        .transactions(
            Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
                .single()
                .expect("ts"),
        )
        .await
        .expect("transactions");
    assert_eq!(transactions.len(), 2);
    let holdings = src.holdings().await.expect("holdings");
    assert!(!holdings.is_empty());
}

#[tokio::test]
async fn since_filters_transactions_by_posted_date() {
    let src = synthetic();
    let all = src
        .transactions(
            Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
                .single()
                .expect("ts"),
        )
        .await
        .expect("all");
    assert_eq!(all.len(), 3);

    let recent = src
        .transactions(
            Utc.with_ymd_and_hms(2026, 7, 10, 0, 0, 0)
                .single()
                .expect("ts"),
        )
        .await
        .expect("recent");
    let ids: Vec<&str> = recent.iter().map(|t| t.external_id.as_str()).collect();
    assert_eq!(
        ids,
        ["txn-0001", "txn-0002"],
        "txn-0003 (2026-07-01) is before `since`"
    );
}

#[tokio::test]
async fn health_reports_last_successful_sync_per_institution() {
    let src = synthetic();
    let health = src.health().await.expect("health");

    // Only the linked institution appears; the manual account does not.
    assert_eq!(health.institutions.len(), 1);
    let first_national = &health.institutions[0];
    assert_eq!(first_national.institution, "First National");
    // The later failed sync must NOT advance the success timestamp.
    assert_eq!(
        first_national.last_successful_sync_at,
        Utc.with_ymd_and_hms(2026, 8, 15, 6, 0, 5).single()
    );
}
