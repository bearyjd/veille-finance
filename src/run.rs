//! One full cycle for one tenant: sync → evaluate → deliver (PRP §10,
//! `veille run --once`). Delivery is symmetric by construction (§2.2): the
//! digest goes to [`crate::deliver::all_recipient_emails`] — owners and
//! watchers together, one send, one audit row each — and push goes to the
//! tenant's single shared topic.

use chrono::{DateTime, Datelike, Utc};

use crate::config::TenantConfig;
use crate::deliver::{EmailSender, PushSender, all_recipient_emails};
use crate::digest::{DigestError, DigestInput, build_digest_input, render_html, render_text};
use crate::rules::{EngineError, evaluate_tenant};
use crate::source::SureSource;
use crate::store::{Store, StoreError, TenantId};
use crate::sync::{SyncError, SyncOptions, sync_tenant};

const DEFAULT_DIGEST_DAY: &str = "sunday";
const DEFAULT_DIGEST_PERIOD_DAYS: u32 = 7;
/// At most this many detailed pushes per run; a backlog beyond it is covered
/// by one summary push (and, always, by the digest).
const MAX_PUSHES_PER_RUN: usize = 10;
/// "All history" for a tenant's first-ever digest.
const FIRST_DIGEST_PERIOD_DAYS: u32 = 3650;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Digest(#[from] DigestError),
}

/// Optional narration hook; `None` in tests and when no LLM is configured.
#[async_trait::async_trait]
pub trait Narrator: Send + Sync {
    async fn narrate(&self, input: &DigestInput) -> Option<String>;
}

#[derive(Default)]
pub struct RunOptions<'a> {
    pub sync: SyncOptions,
    pub narrator: Option<&'a dyn Narrator>,
}

#[derive(Debug, Default)]
pub struct RunOutcome {
    pub findings_active: usize,
    pub alerts_pushed: usize,
    pub digest_sent: bool,
    /// The sync failed; evaluation and delivery ran from stored state. A dead
    /// upstream is exactly when sync-stale alerts must still go out.
    pub sync_error: Option<String>,
    /// Delivery attempts that failed this run — surfaced so the timer exits
    /// nonzero instead of reporting silent success.
    pub delivery_failures: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tenant_once(
    store: &Store,
    tenant: TenantId,
    tenant_config: &TenantConfig,
    source: &dyn SureSource,
    push: Option<&dyn PushSender>,
    email: Option<&dyn EmailSender>,
    now: DateTime<Utc>,
    options: RunOptions<'_>,
) -> Result<RunOutcome, RunError> {
    // A failed sync must not starve delivery: the store still holds the
    // last-known state, stale health makes sync-stale fire, and that alert
    // going out is the whole point of this tool.
    let sync_error = match sync_tenant(store, tenant, source, now, options.sync).await {
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(tenant = ?tenant, error = %e, "sync failed; delivering from stored state");
            Some(e.to_string())
        }
    };
    let findings = evaluate_tenant(store, tenant, tenant_config.rules.clone(), now, true).await?;

    let mut outcome = RunOutcome {
        findings_active: findings.len(),
        sync_error,
        ..RunOutcome::default()
    };

    // Immediate channel: Alert severity only (PRP §9). A failed push stays
    // owed and retries next run; it never blocks the rest of the cycle.
    // Volume is capped: a backlog (first backfill, outage recovery, rule
    // retuning) sends at most MAX_PUSHES_PER_RUN detailed pushes plus one
    // summary covering the rest — full detail is always in the digest.
    if let Some(push) = push {
        // ASCII title: header values travel through proxies unmangled.
        let title: String = format!("{} - veille alert", tenant_config.display_name)
            .chars()
            .map(|c| if c.is_ascii() { c } else { '?' })
            .collect();
        let owed = store.unpushed_alerts(tenant).await?;
        let (detailed, remainder) = owed.split_at(owed.len().min(MAX_PUSHES_PER_RUN));

        let mut channel_healthy = true;
        for finding in detailed {
            match push.send(&title, &finding.summary).await {
                Ok(()) => {
                    // Atomic claim + audit; false means another run recorded
                    // it first (duplicate external send, single audit row).
                    if store
                        .record_push(tenant, &finding.dedupe_key, &push.destination(), now)
                        .await?
                    {
                        outcome.alerts_pushed += 1;
                    }
                }
                Err(reason) => {
                    // First failure = unhealthy endpoint (or rate limit):
                    // stop pushing this run; everything unsent stays owed.
                    tracing::warn!(
                        tenant = ?tenant,
                        dedupe_key = %finding.dedupe_key,
                        %reason,
                        "alert push failed; remaining alerts stay owed for next run"
                    );
                    outcome
                        .delivery_failures
                        .push(format!("push {}: {reason}", finding.dedupe_key));
                    channel_healthy = false;
                    break;
                }
            }
        }

        if channel_healthy && !remainder.is_empty() {
            let body = format!(
                "{} more alerts also fired; full details are in the digest.",
                remainder.len()
            );
            match push.send(&title, &body).await {
                Ok(()) => {
                    let keys: Vec<String> =
                        remainder.iter().map(|f| f.dedupe_key.clone()).collect();
                    let claimed = store
                        .record_push_batch(tenant, &keys, &push.destination(), now)
                        .await?;
                    outcome.alerts_pushed += claimed.len();
                }
                Err(reason) => {
                    tracing::warn!(tenant = ?tenant, %reason, "summary push failed");
                    outcome
                        .delivery_failures
                        .push(format!("push summary: {reason}"));
                }
            }
        }
    }

    // Periodic channel: the digest, on its configured weekday, once per day.
    let digest_day = tenant_config
        .digest_day
        .as_deref()
        .unwrap_or(DEFAULT_DIGEST_DAY)
        .to_lowercase();
    let today_matches = weekday_name(now) == digest_day;
    if let Some(email) = email
        && today_matches
        && !tenant_config.recipients.is_empty()
        && !store.smtp_delivered_on(tenant, now.date_naive()).await?
    {
        // The coverage window is structural, not purely configured: it always
        // extends back to the last recorded digest (all history for the
        // first), so no digest_period_days / digest_day combination can open
        // a gap that hides a pushed alert from the owners (§2.2).
        let configured_days = tenant_config
            .digest_period_days
            .unwrap_or(DEFAULT_DIGEST_PERIOD_DAYS);
        let period_days = match store.last_smtp_delivery_at(tenant).await? {
            Some(last) => {
                let gap_days = (now.date_naive() - last.date_naive()).num_days().max(0) as u32 + 1;
                configured_days.max(gap_days).min(FIRST_DIGEST_PERIOD_DAYS)
            }
            None => FIRST_DIGEST_PERIOD_DAYS,
        };
        let mut input =
            build_digest_input(store, tenant, &tenant_config.display_name, period_days, now)
                .await?;
        if let Some(narrator) = options.narrator {
            input.narration = narrator.narrate(&input).await;
        }
        let text = render_text(&input)?;
        let html = render_html(&input)?;
        let subject = format!(
            "{} — financial watch digest, {} to {}",
            tenant_config.display_name, input.period_start, input.period_end
        );
        // §2.2: one send, everyone. There is no per-role path to take here.
        let to = all_recipient_emails(&tenant_config.recipients);
        match email.send(&to, &subject, &text, &html).await {
            Ok(()) => {
                let finding_keys: Vec<String> = input
                    .findings
                    .iter()
                    .map(|f| f.dedupe_key.clone())
                    .collect();
                // All recipient rows in one transaction: a partial commit
                // could otherwise record a watcher without the owners.
                store
                    .record_smtp_deliveries(tenant, &to, &finding_keys, now)
                    .await?;
                outcome.digest_sent = true;
            }
            Err(reason) => {
                // No audit rows on failure: the next run today retries.
                tracing::warn!(tenant = ?tenant, %reason, "digest send failed; will retry");
                outcome.delivery_failures.push(format!("digest: {reason}"));
            }
        }
    }

    Ok(outcome)
}

fn weekday_name(now: DateTime<Utc>) -> &'static str {
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
