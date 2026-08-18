#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Phase 4 gate (PRP §11): the symmetric-visibility test — no delivery path
//! can send a finding to a watcher without sending it to the tenant's owners
//! — plus push/digest delivery semantics and the audit trail.

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{Datelike, TimeZone, Utc};
use tempfile::TempDir;
use veille::config::{RecipientConfig, TenantConfig, UpstreamConfig};
use veille::deliver::{EmailSender, PushSender};
use veille::run::{RunOptions, run_tenant_once};
use veille::source::fixture::FixtureSureSource;
use veille::store::{Store, TenantId};

#[derive(Default)]
struct FakePush {
    sent: Mutex<Vec<(String, String)>>,
    fail: Mutex<bool>,
}

#[async_trait]
impl PushSender for FakePush {
    async fn send(&self, title: &str, body: &str) -> Result<(), String> {
        if *self.fail.lock().expect("lock") {
            return Err("push endpoint down".into());
        }
        self.sent
            .lock()
            .expect("lock")
            .push((title.to_string(), body.to_string()));
        Ok(())
    }

    fn destination(&self) -> String {
        "fake-topic".into()
    }
}

#[derive(Default)]
struct FakeEmail {
    sent: Mutex<Vec<(Vec<String>, String)>>,
}

#[async_trait]
impl EmailSender for FakeEmail {
    async fn send(
        &self,
        to: &[String],
        subject: &str,
        _text: &str,
        _html: &str,
    ) -> Result<(), String> {
        self.sent
            .lock()
            .expect("lock")
            .push((to.to_vec(), subject.to_string()));
        Ok(())
    }
}

fn recipient(name: &str, role: &str, email: &str) -> RecipientConfig {
    RecipientConfig {
        name: name.into(),
        role: role.into(),
        email: email.into(),
    }
}

fn tenant_config(digest_day: Option<&str>) -> TenantConfig {
    TenantConfig {
        slug: "parents".into(),
        display_name: "Parents".into(),
        lookback_days: None,
        upstream: UpstreamConfig {
            base_url: "http://unused:1".into(),
            api_key_env: "UNUSED".into(),
        },
        rules: Default::default(),
        digest_period_days: None,
        digest_day: digest_day.map(String::from),
        recipients: vec![
            recipient("Mom", "owner", "mom@example.com"),
            recipient("Dad", "owner", "dad@example.com"),
            recipient("JD", "watcher", "jd@example.com"),
        ],
        push: None,
    }
}

async fn fresh_store(dir: &TempDir) -> (Store, TenantId) {
    let store = Store::open(&dir.path().join("t.sqlite3"))
        .await
        .expect("store");
    let tenant = store
        .tenants()
        .ensure("parents", "Parents")
        .await
        .expect("tenant");
    (store, tenant)
}

fn fixture_source() -> FixtureSureSource {
    FixtureSureSource::new(format!("{}/fixtures/synthetic", env!("CARGO_MANIFEST_DIR")))
}

/// `now` such that the fixture's stale health fires sync-stale (Alert), and
/// whose weekday we can match digest_day against.
fn eval_now() -> chrono::DateTime<Utc> {
    // 2026-08-20 is a Thursday.
    Utc.with_ymd_and_hms(2026, 8, 20, 22, 0, 0)
        .single()
        .expect("ts")
}

fn weekday_name(now: chrono::DateTime<Utc>) -> &'static str {
    match now.weekday() {
        chrono::Weekday::Mon => "monday",
        chrono::Weekday::Tue => "tuesday",
        chrono::Weekday::Wed => "wednesday",
        chrono::Weekday::Thu => "thursday",
        chrono::Weekday::Fri => "friday",
        chrono::Weekday::Sat => "saturday",
        chrono::Weekday::Sun => "sunday",
    }
}

#[tokio::test]
async fn digest_email_reaches_owners_and_watchers_in_one_send() {
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    let config = tenant_config(Some(weekday_name(now)));

    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run");

    let to = {
        let sent = email.sent.lock().expect("lock");
        assert_eq!(sent.len(), 1, "one digest send for everyone");
        sent[0].0.clone()
    };
    for addr in ["mom@example.com", "dad@example.com", "jd@example.com"] {
        assert!(
            to.contains(&addr.to_string()),
            "§2.2: owners and watchers receive the same digest: {to:?}"
        );
    }

    // Audit trail: one delivery row per recipient, identical finding sets.
    let rows = store.deliveries(tenant).await.expect("deliveries");
    let smtp_rows: Vec<_> = rows.iter().filter(|r| r.channel == "smtp").collect();
    assert_eq!(smtp_rows.len(), 3);
    let first_ids = &smtp_rows[0].finding_keys;
    assert!(!first_ids.is_empty(), "the digest covered findings");
    for row in &smtp_rows {
        assert_eq!(
            &row.finding_keys, first_ids,
            "watchers and owners must see the identical finding set"
        );
    }
}

#[tokio::test]
async fn watcher_digest_rows_always_have_matching_owner_rows() {
    // The structural half of the gate: however findings and channels are
    // arranged, a watcher row in the audit trail implies owner rows with the
    // same finding set in the same cycle.
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    let config = tenant_config(Some(weekday_name(now)));

    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run");

    let rows = store.deliveries(tenant).await.expect("deliveries");
    let watcher_rows: Vec<_> = rows
        .iter()
        .filter(|r| r.recipient == "jd@example.com")
        .collect();
    assert!(!watcher_rows.is_empty(), "watcher received the cycle");
    for watcher_row in watcher_rows {
        for owner in ["mom@example.com", "dad@example.com"] {
            assert!(
                rows.iter().any(|r| r.recipient == owner
                    && r.channel == watcher_row.channel
                    && r.finding_keys == watcher_row.finding_keys),
                "no delivery to a watcher without the same delivery to every owner"
            );
        }
    }
}

#[tokio::test]
async fn alerts_push_once_and_only_alerts() {
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    // No digest today: isolate the push path.
    let config = tenant_config(Some("monday"));

    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run 1");

    {
        let sent = push.sent.lock().expect("lock");
        assert_eq!(sent.len(), 1, "exactly the sync-stale Alert: {sent:?}");
        assert!(sent[0].1.contains("First National"));
    }
    assert!(
        email.sent.lock().expect("lock").is_empty(),
        "not digest day"
    );

    // Second run: the same alert is still active but already pushed.
    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now + chrono::Duration::hours(1),
        RunOptions::default(),
    )
    .await
    .expect("run 2");
    assert_eq!(
        push.sent.lock().expect("lock").len(),
        1,
        "an already-pushed alert must not re-push"
    );
}

#[tokio::test]
async fn failed_push_is_retried_on_the_next_run() {
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    let config = tenant_config(Some("monday"));

    *push.fail.lock().expect("lock") = true;
    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run survives push failure");
    assert!(push.sent.lock().expect("lock").is_empty());

    *push.fail.lock().expect("lock") = false;
    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now + chrono::Duration::hours(1),
        RunOptions::default(),
    )
    .await
    .expect("run 2");
    assert_eq!(
        push.sent.lock().expect("lock").len(),
        1,
        "an alert whose push failed is still owed a push"
    );
}

#[tokio::test]
async fn digest_sends_once_per_day_even_if_run_twice() {
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    let config = tenant_config(Some(weekday_name(now)));

    for offset_hours in [0, 2] {
        run_tenant_once(
            &store,
            tenant,
            &config,
            &fixture_source(),
            Some(&push),
            Some(&email),
            now + chrono::Duration::hours(offset_hours),
            RunOptions::default(),
        )
        .await
        .expect("run");
    }
    assert_eq!(
        email.sent.lock().expect("lock").len(),
        1,
        "a timer double-fire must not double-send the digest"
    );
}

mod ntfy_transport {
    use veille::deliver::PushSender;
    use veille::deliver::push::NtfyPush;
    use wiremock::matchers::{body_string, header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn posts_title_priority_body_and_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Title", "Parents - veille alert"))
            .and(header("Priority", "high"))
            .and(header("Authorization", "Bearer topic-token"))
            .and(body_string(
                "No successful sync from First National in 5 days (threshold 4).",
            ))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let push = NtfyPush::new(server.uri(), Some("topic-token".into())).expect("push");
        push.send(
            "Parents - veille alert",
            "No successful sync from First National in 5 days (threshold 4).",
        )
        .await
        .expect("send ok");
    }

    #[tokio::test]
    async fn non_success_is_an_error_and_redirects_are_refused() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let push = NtfyPush::new(server.uri(), None).expect("push");
        let err = push.send("t", "b").await.expect_err("500 must fail");
        assert!(err.contains("500"), "{err}");

        let attacker = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&attacker)
            .await;
        let server2 = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", format!("{}/x", attacker.uri()).as_str()),
            )
            .mount(&server2)
            .await;
        let push = NtfyPush::new(server2.uri(), Some("secret".into())).expect("push");
        assert!(push.send("t", "b").await.is_err(), "redirect must fail");
        assert!(
            attacker.received_requests().await.expect("reqs").is_empty(),
            "the bearer token must never follow a redirect"
        );
    }
}

/// A source whose every call fails — the upstream is down.
struct DeadSource;

#[async_trait]
impl veille::source::SureSource for DeadSource {
    async fn accounts(&self) -> veille::source::Result<Vec<veille::domain::Account>> {
        Err(veille::source::SourceError::Request("upstream down".into()))
    }
    async fn transactions(
        &self,
        _since: chrono::DateTime<Utc>,
    ) -> veille::source::Result<Vec<veille::domain::Transaction>> {
        Err(veille::source::SourceError::Request("upstream down".into()))
    }
    async fn holdings(&self) -> veille::source::Result<Vec<veille::domain::Holding>> {
        Err(veille::source::SourceError::Request("upstream down".into()))
    }
    async fn health(&self) -> veille::source::Result<veille::domain::UpstreamHealth> {
        Err(veille::source::SourceError::Request("upstream down".into()))
    }
}

#[tokio::test]
async fn a_dead_upstream_does_not_starve_owed_deliveries() {
    // The failure mode this tool exists for: the upstream dies, the stored
    // health goes stale, sync-stale fires — and the alert must still go out
    // even though sync itself fails.
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let config = tenant_config(Some("monday"));

    // Seed the store from a working upstream first.
    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        None, // no push yet: leave the alert owed
        Some(&email),
        eval_now(),
        RunOptions::default(),
    )
    .await
    .expect("seed run");

    // Now the upstream is dead. The owed alert must still push.
    let outcome = run_tenant_once(
        &store,
        tenant,
        &config,
        &DeadSource,
        Some(&push),
        Some(&email),
        eval_now() + chrono::Duration::hours(1),
        RunOptions::default(),
    )
    .await
    .expect("run tolerates a dead upstream");
    assert!(outcome.sync_error.is_some(), "the sync failure is reported");
    assert_eq!(
        push.sent.lock().expect("lock").len(),
        1,
        "owed alerts deliver from stored state even when sync fails"
    );
}

#[tokio::test]
async fn failed_digest_sends_write_no_audit_rows_and_are_reported() {
    #[derive(Default)]
    struct FailingEmail;
    #[async_trait]
    impl EmailSender for FailingEmail {
        async fn send(&self, _: &[String], _: &str, _: &str, _: &str) -> Result<(), String> {
            Err("smtp rejected".into())
        }
    }

    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FailingEmail;
    let now = eval_now();
    let config = tenant_config(Some(weekday_name(now)));

    let outcome = run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run survives");
    assert!(!outcome.digest_sent);
    assert!(
        !outcome.delivery_failures.is_empty(),
        "a due-but-failed digest must be visible to the timer"
    );
    let rows = store.deliveries(tenant).await.expect("rows");
    assert!(
        rows.iter().all(|r| r.channel != "smtp"),
        "no audit rows for a send that did not happen"
    );
}

#[tokio::test]
async fn no_digest_window_configuration_can_hide_a_pushed_alert_from_owners() {
    // The window-gap attack: period 1 day, digest on Sunday. An alert seen
    // only on Tuesday must still appear in the next digest, because the
    // window extends to the last recorded digest (all history for the first).
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();

    let mut config = tenant_config(Some("sunday"));
    config.digest_period_days = Some(1);

    // Tuesday 2026-08-25 (10 days after the fixture's last successful sync):
    // the alert fires and is pushed. No digest due.
    let tuesday = Utc
        .with_ymd_and_hms(2026, 8, 25, 22, 0, 0)
        .single()
        .expect("ts");
    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        tuesday,
        RunOptions::default(),
    )
    .await
    .expect("tuesday run");
    assert_eq!(
        push.sent.lock().expect("lock").len(),
        1,
        "alert pushed on Tuesday"
    );
    assert!(email.sent.lock().expect("lock").is_empty());

    // Sunday 2026-08-30: first-ever digest. The Tuesday alert must be in it.
    let sunday = Utc
        .with_ymd_and_hms(2026, 8, 30, 22, 0, 0)
        .single()
        .expect("ts");
    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        sunday,
        RunOptions::default(),
    )
    .await
    .expect("sunday run");

    {
        let sent = email.sent.lock().expect("lock");
        assert_eq!(sent.len(), 1, "digest went out");
    }
    let rows = store.deliveries(tenant).await.expect("rows");
    let owner_row = rows
        .iter()
        .find(|r| r.channel == "smtp" && r.recipient == "mom@example.com")
        .expect("owner digest row");
    assert!(
        owner_row
            .finding_keys
            .iter()
            .any(|k| k.starts_with("sync-stale")),
        "§2.2: the pushed alert must reach the owners' digest regardless of \
         the configured window: {owner_row:?}"
    );
}

#[tokio::test]
async fn push_volume_is_capped_with_a_summary_covering_the_rest() {
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    let config = tenant_config(Some("monday"));

    // A backlog of 14 owed alerts.
    let findings: Vec<veille::domain::Finding> = (0..14)
        .map(|i| veille::domain::Finding {
            rule_id: "large-transfer".into(),
            severity: veille::domain::Severity::Alert,
            subject: format!("account:a{i}"),
            summary: format!("Large transaction number {i}."),
            evidence: serde_json::json!({}),
            dedupe_key: format!("large-transfer:backlog-{i:02}"),
            detected_at: now,
        })
        .collect();
    store
        .upsert_findings(tenant, &findings)
        .await
        .expect("seed");

    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run");

    {
        let sent = push.sent.lock().expect("lock");
        assert!(
            sent.len() <= 11,
            "at most the cap plus one summary push, got {}",
            sent.len()
        );
        let summary = &sent[sent.len() - 1].1;
        assert!(
            summary.contains("more alert"),
            "the tail is summarized, not dropped: {summary}"
        );
    }

    // Every owed alert is accounted for: nothing left unpushed.
    let rows = store.deliveries(tenant).await.expect("rows");
    let pushed_keys: std::collections::BTreeSet<String> = rows
        .iter()
        .filter(|r| r.channel == "push")
        .flat_map(|r| r.finding_keys.iter().cloned())
        .collect();
    for finding in &findings {
        assert!(
            pushed_keys.contains(&finding.dedupe_key),
            "{} must be covered by a detailed or summary push",
            finding.dedupe_key
        );
    }
}

#[tokio::test]
async fn a_push_audit_row_implies_owner_digest_rows_for_the_same_findings() {
    // The gate, extended to the channel the audit-trail test was blind to:
    // any pushed finding must appear in every owner's digest row once a
    // digest goes out.
    let dir = TempDir::new().expect("tempdir");
    let (store, tenant) = fresh_store(&dir).await;
    let push = FakePush::default();
    let email = FakeEmail::default();
    let now = eval_now();
    let config = tenant_config(Some(weekday_name(now)));

    run_tenant_once(
        &store,
        tenant,
        &config,
        &fixture_source(),
        Some(&push),
        Some(&email),
        now,
        RunOptions::default(),
    )
    .await
    .expect("run");

    let rows = store.deliveries(tenant).await.expect("rows");
    let push_keys: Vec<&String> = rows
        .iter()
        .filter(|r| r.channel == "push")
        .flat_map(|r| r.finding_keys.iter())
        .collect();
    assert!(!push_keys.is_empty(), "the alert was pushed");
    for owner in ["mom@example.com", "dad@example.com"] {
        let owner_keys: std::collections::BTreeSet<&String> = rows
            .iter()
            .filter(|r| r.channel == "smtp" && r.recipient == owner)
            .flat_map(|r| r.finding_keys.iter())
            .collect();
        for key in &push_keys {
            assert!(
                owner_keys.contains(key),
                "pushed finding {key} missing from {owner}'s digest row"
            );
        }
    }
}
