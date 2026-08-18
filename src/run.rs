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
    sync_tenant(store, tenant, source, now, options.sync).await?;
    let findings = evaluate_tenant(store, tenant, tenant_config.rules.clone(), now, true).await?;

    let mut outcome = RunOutcome {
        findings_active: findings.len(),
        ..RunOutcome::default()
    };

    // Immediate channel: Alert severity only (PRP §9). A failed push stays
    // owed and retries next run; it never blocks the rest of the cycle.
    if let Some(push) = push {
        for finding in store.unpushed_alerts(tenant).await? {
            // ASCII title: header values travel through proxies unmangled.
            let title = format!("{} - veille alert", tenant_config.display_name);
            match push.send(&title, &finding.summary).await {
                Ok(()) => {
                    store.mark_pushed(tenant, &finding.dedupe_key, now).await?;
                    store
                        .record_delivery(
                            tenant,
                            "push",
                            &push.destination(),
                            std::slice::from_ref(&finding.dedupe_key),
                            now,
                        )
                        .await?;
                    outcome.alerts_pushed += 1;
                }
                Err(reason) => {
                    tracing::warn!(
                        tenant = ?tenant,
                        dedupe_key = %finding.dedupe_key,
                        %reason,
                        "alert push failed; will retry next run"
                    );
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
        let period_days = tenant_config
            .digest_period_days
            .unwrap_or(DEFAULT_DIGEST_PERIOD_DAYS);
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
                for recipient in &to {
                    store
                        .record_delivery(tenant, "smtp", recipient, &finding_keys, now)
                        .await?;
                }
                outcome.digest_sent = true;
            }
            Err(reason) => {
                // No audit rows on failure: the next run today retries.
                tracing::warn!(tenant = ?tenant, %reason, "digest send failed; will retry");
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
