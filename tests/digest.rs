#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Phase 3 gate: the digest renders correctly with the LLM endpoint
//! unreachable and with none configured (PRP §11). The plain templated
//! rendering of findings IS the product; narration is garnish.

use chrono::{TimeZone, Utc};
use tempfile::TempDir;
use veille::config::RuleThresholds;
use veille::digest::{build_digest_input, render_html, render_text};
use veille::domain::format_minor;
use veille::rules::evaluate_tenant;
use veille::source::fixture::FixtureSureSource;
use veille::store::{Store, TenantId};
use veille::sync::{SyncOptions, sync_tenant};

#[test]
fn money_formats_with_symbol_grouping_and_exponent() {
    assert_eq!(format_minor(43_014, "USD"), "$430.14");
    assert_eq!(format_minor(-1_800_000, "USD"), "-$18,000.00");
    assert_eq!(format_minor(123_456, "GBP"), "£1,234.56");
    assert_eq!(format_minor(1_500, "JPY"), "¥1,500");
    assert_eq!(format_minor(0, "USD"), "$0.00");
    // Unknown symbol: fall back to the ISO code, never guess a symbol.
    assert_eq!(format_minor(123_456, "SEK"), "SEK 1,234.56");
}

async fn store_with_findings() -> (TempDir, Store, TenantId) {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store
        .tenants()
        .ensure("a", "Alpha Household")
        .await
        .expect("tenant");
    let source =
        FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")));
    let sync_now = Utc
        .with_ymd_and_hms(2026, 8, 17, 22, 0, 0)
        .single()
        .expect("ts");
    sync_tenant(&store, tenant, &source, sync_now, SyncOptions::default())
        .await
        .expect("sync");
    // Persist findings: the stale-health alert exists at Aug 20.
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    evaluate_tenant(&store, tenant, RuleThresholds::default(), as_of, true)
        .await
        .expect("evaluate");
    (dir, store, tenant)
}

#[tokio::test]
async fn digest_renders_without_any_llm() {
    let (_dir, store, tenant) = store_with_findings().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");

    let input = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("digest input");
    // No narration configured: input.narration is None and rendering must
    // still be complete.
    assert!(input.narration.is_none());

    let text = render_text(&input).expect("text renders");
    assert!(text.contains("Alpha Household"));
    assert!(
        text.contains("sync-stale"),
        "findings must be listed: {text}"
    );
    assert!(text.contains("First National"));
    assert!(
        text.contains("ALERT") || text.contains("alert"),
        "severity must be visible: {text}"
    );
    // Snapshot deltas: the fixture accounts appear with formatted balances.
    assert!(text.contains("Everyday Checking"));
    assert!(text.contains("$2,500.00"));

    let html = render_html(&input).expect("html renders");
    assert!(html.contains("Alpha Household"));
    assert!(html.contains("First National"));
}

#[tokio::test]
async fn digest_renders_with_zero_findings() {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store
        .tenants()
        .ensure("a", "Quiet Household")
        .await
        .expect("tenant");
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");

    let input = build_digest_input(&store, tenant, "Quiet Household", 7, as_of)
        .await
        .expect("digest input");
    let text = render_text(&input).expect("text renders");
    assert!(
        text.to_lowercase().contains("no findings"),
        "an empty period must say so plainly: {text}"
    );
}

#[tokio::test]
async fn digest_is_deterministic() {
    let (_dir, store, tenant) = store_with_findings().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let a = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("a");
    let b = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("b");
    assert_eq!(render_text(&a).expect("ra"), render_text(&b).expect("rb"));
}

#[tokio::test]
async fn html_escapes_hostile_finding_content() {
    // Transaction descriptions come from upstream/bank data — untrusted.
    let (_dir, store, tenant) = store_with_findings().await;
    let hostile = veille::domain::Finding {
        rule_id: "large-transfer".into(),
        severity: veille::domain::Severity::Alert,
        subject: "account:<script>alert(1)</script>".into(),
        evidence: serde_json::json!({ "description": "<img src=x onerror=alert(1)>" }),
        dedupe_key: "large-transfer:hostile".into(),
        detected_at: Utc
            .with_ymd_and_hms(2026, 8, 20, 12, 0, 0)
            .single()
            .expect("ts"),
    };
    store
        .upsert_findings(tenant, &[hostile])
        .await
        .expect("insert");

    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let input = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("input");
    let html = render_html(&input).expect("html");
    assert!(
        !html.contains("<script>") && !html.contains("<img"),
        "no unescaped tag from finding content may survive into HTML"
    );
    assert!(
        html.contains("&lt;script&gt;"),
        "the hostile content should be present but escaped"
    );
}
