//! Digest assembly and rendering (PRP §8–9). The plain templated rendering
//! of findings is the product; LLM narration, when present, is an extra
//! section. Rendering must never depend on the LLM being configured or
//! reachable.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde_json::json;

use crate::domain::format_minor;
use crate::store::repo::StoredFinding;
use crate::store::{Store, StoreError, TenantId};

const TEXT_TEMPLATE: &str = include_str!("deliver/templates/digest.txt");
const HTML_TEMPLATE: &str = include_str!("deliver/templates/digest.html");

#[derive(Debug, thiserror::Error)]
pub enum DigestError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("template error: {0}")]
    Template(#[from] minijinja::Error),
}

/// One account's balance movement over the digest period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDelta {
    pub account: String,
    /// Balance at (or before) the period start; `None` when the account has
    /// no observation that old, or none within the period.
    pub start_minor: Option<i64>,
    pub end_minor: i64,
    pub currency: String,
    /// Date of the newest observation; before `period_start` means the
    /// balance shown is stale.
    pub last_observed: NaiveDate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DigestInput {
    pub tenant_display_name: String,
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
    /// Sorted most severe first, then rule id, then dedupe key.
    pub findings: Vec<StoredFinding>,
    pub deltas: Vec<AccountDelta>,
    /// LLM prose, if narration ran and succeeded. Never load-bearing.
    pub narration: Option<String>,
}

/// Gather everything the digest shows for one tenant: findings detected in
/// the period and per-account balance movement.
pub async fn build_digest_input(
    store: &Store,
    tenant: TenantId,
    display_name: &str,
    period_days: u32,
    as_of: DateTime<Utc>,
) -> Result<DigestInput, DigestError> {
    let period_end = as_of.date_naive();
    let period_start = period_end - Duration::days(i64::from(period_days));
    let since = period_start
        .and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc())
        .unwrap_or(DateTime::<Utc>::MIN_UTC);

    let findings = store.findings_active_in(tenant, since, as_of).await?;

    // Latest observation per account up to as_of, and the last observation
    // at or before period_start for the delta.
    struct Series {
        external_id: String,
        name: String,
        currency: String,
        start_minor: Option<i64>,
        end_minor: i64,
        last_observed: NaiveDate,
    }
    let mut per_account: BTreeMap<String, Series> = BTreeMap::new();
    for row in store.account_snapshot_rows(tenant, as_of).await? {
        let entry = per_account
            .entry(row.external_id.clone())
            .or_insert(Series {
                external_id: row.external_id.clone(),
                name: row.name.clone(),
                currency: row.currency.clone(),
                start_minor: None,
                end_minor: row.balance_minor,
                last_observed: row.as_of_date,
            });
        // Rows arrive ascending, so the last write per bucket wins.
        entry.name = row.name;
        entry.currency = row.currency;
        entry.end_minor = row.balance_minor;
        entry.last_observed = row.as_of_date;
        if row.as_of_date <= period_start {
            entry.start_minor = Some(row.balance_minor);
        }
    }

    // Two accounts can share a display name; disambiguate so the digest never
    // shows two indistinguishable rows.
    let mut name_counts: BTreeMap<&str, u32> = BTreeMap::new();
    for series in per_account.values() {
        *name_counts.entry(series.name.as_str()).or_default() += 1;
    }
    let ambiguous: std::collections::BTreeSet<String> = name_counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, _)| name.to_string())
        .collect();

    let mut deltas: Vec<AccountDelta> = per_account
        .into_values()
        .map(|s| {
            let account = if ambiguous.contains(&s.name) {
                let tail: String = s
                    .external_id
                    .chars()
                    .rev()
                    .take(4)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("{} (\u{2026}{tail})", s.name)
            } else {
                s.name
            };
            AccountDelta {
                account,
                // A single stale observation is "no data", not "no change":
                // only report a delta when the account was actually observed
                // during the period.
                start_minor: (s.last_observed > period_start)
                    .then_some(s.start_minor)
                    .flatten(),
                end_minor: s.end_minor,
                currency: s.currency,
                last_observed: s.last_observed,
            }
        })
        .collect();
    deltas.sort_by(|a, b| a.account.cmp(&b.account));

    Ok(DigestInput {
        tenant_display_name: display_name.to_string(),
        period_start,
        period_end,
        findings,
        deltas,
        narration: None,
    })
}

fn template_context(input: &DigestInput) -> minijinja::Value {
    let findings: Vec<serde_json::Value> = input
        .findings
        .iter()
        .map(|f| {
            json!({
                "severity": f.severity.as_str().to_uppercase(),
                "rule_id": f.rule_id,
                "summary": f.summary,
                "subject": f.subject,
                "evidence": f.evidence.to_string(),
            })
        })
        .collect();
    let deltas: Vec<serde_json::Value> = input
        .deltas
        .iter()
        .map(|d| {
            let change = d.start_minor.map(|start| {
                let diff = d.end_minor.saturating_sub(start);
                let formatted = format_minor(diff, &d.currency);
                if diff >= 0 {
                    format!("+{formatted}")
                } else {
                    formatted
                }
            });
            let stale_since =
                (d.last_observed < input.period_start).then(|| d.last_observed.to_string());
            json!({
                "account": d.account,
                "end": format_minor(d.end_minor, &d.currency),
                "change": change,
                "stale_since": stale_since,
            })
        })
        .collect();
    // Narration paragraphs are rendered individually so the HTML template
    // can wrap each in its own escaped <p>.
    let narration_paragraphs: Option<Vec<String>> = input.narration.as_ref().map(|n| {
        n.split("\n\n")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    });
    minijinja::Value::from_serialize(json!({
        "tenant": input.tenant_display_name,
        "period_start": input.period_start.to_string(),
        "period_end": input.period_end.to_string(),
        "narration_paragraphs": narration_paragraphs,
        "findings": findings,
        "deltas": deltas,
    }))
}

fn render(template_name: &str, source: &str, input: &DigestInput) -> Result<String, DigestError> {
    let mut env = minijinja::Environment::new();
    // Template names carry extensions so minijinja auto-escapes the HTML one.
    env.add_template(template_name, source)?;
    let rendered = env
        .get_template(template_name)?
        .render(template_context(input))?;
    Ok(rendered)
}

pub fn render_text(input: &DigestInput) -> Result<String, DigestError> {
    render("digest.txt", TEXT_TEMPLATE, input)
}

pub fn render_html(input: &DigestInput) -> Result<String, DigestError> {
    render("digest.html", HTML_TEMPLATE, input)
}
