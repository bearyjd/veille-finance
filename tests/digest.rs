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
        text.contains("No successful sync from First National"),
        "findings must be listed as human sentences: {text}"
    );
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
        summary: "Hostile <b>summary</b> content.".into(),
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
        html.contains("&lt;b&gt;") || html.contains("&lt;img"),
        "the hostile content should be present but escaped: {html}"
    );
}

#[tokio::test]
async fn an_ongoing_alert_stays_in_every_digest_until_it_clears() {
    let (_dir, store, tenant) = store_with_findings().await;

    // A week later the outage persists: evaluate re-detects the same episode
    // (same dedupe key — no new row, but the condition is still active).
    let later = Utc
        .with_ymd_and_hms(2026, 8, 27, 22, 0, 0)
        .single()
        .expect("ts");
    evaluate_tenant(&store, tenant, RuleThresholds::default(), later, true)
        .await
        .expect("re-evaluate");

    // The Aug 27 digest window (Aug 20–27) contains no NEW detection, but the
    // outage is ongoing and must be shown.
    let input = build_digest_input(&store, tenant, "Alpha Household", 7, later)
        .await
        .expect("digest input");
    let text = render_text(&input).expect("render");
    assert!(
        text.contains("First National"),
        "an unresolved alert must not vanish from later digests: {text}"
    );
}

#[tokio::test]
async fn digest_text_is_human_readable_not_json() {
    let (_dir, store, tenant) = store_with_findings().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let input = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("input");
    let text = render_text(&input).expect("render");
    assert!(
        text.contains("No successful sync from First National"),
        "each finding needs a human sentence: {text}"
    );
    assert!(
        !text.contains("{\""),
        "raw JSON is not a digest for humans: {text}"
    );
}

#[tokio::test]
async fn hostile_narration_is_escaped_in_html() {
    let (_dir, store, tenant) = store_with_findings().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let mut input = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("input");
    input.narration = Some("All fine.<script>alert(1)</script>\n\nSecond paragraph.".into());
    let html = render_html(&input).expect("html");
    assert!(
        !html.contains("<script>"),
        "narration is untrusted model output and must be escaped: {html}"
    );
    assert!(html.contains("&lt;script&gt;"), "escaped, not dropped");
}

#[tokio::test]
async fn narration_sits_below_the_findings_and_is_labeled() {
    let (_dir, store, tenant) = store_with_findings().await;
    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let mut input = build_digest_input(&store, tenant, "Alpha Household", 7, as_of)
        .await
        .expect("input");
    input.narration = Some("A quiet week overall.".into());
    let text = render_text(&input).expect("render");
    let findings_at = text.find("Findings").expect("findings section");
    let narration_at = text
        .find("A quiet week overall.")
        .expect("narration present");
    assert!(
        narration_at > findings_at,
        "prose is garnish: the deterministic findings come first: {text}"
    );
    assert!(
        text.contains("Automated summary"),
        "machine-generated prose must be labeled as such: {text}"
    );
}

#[tokio::test]
async fn an_account_with_no_data_in_the_period_is_marked_stale_not_unchanged() {
    let dir = TempDir::new().expect("tempdir");
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store.tenants().ensure("a", "A").await.expect("tenant");

    // One observation, long before the digest period.
    let old = Utc
        .with_ymd_and_hms(2026, 6, 1, 22, 0, 0)
        .single()
        .expect("ts");
    store
        .insert_account_snapshots(
            tenant,
            &[veille::domain::Account {
                external_id: "acct-old".into(),
                name: "Dormant Savings".into(),
                institution: None,
                kind: "depository".into(),
                status: "active".into(),
                balance_minor: 100_000,
                currency: "USD".into(),
            }],
            old,
        )
        .await
        .expect("snapshot");

    let as_of = Utc
        .with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts");
    let input = build_digest_input(&store, tenant, "A", 7, as_of)
        .await
        .expect("input");
    let text = render_text(&input).expect("render");
    assert!(
        !text.contains("+$0.00"),
        "one stale observation is 'no data', not 'no change': {text}"
    );
    assert!(
        text.contains("2026-06-01"),
        "the last-observed date must be visible for a stale account: {text}"
    );
}
