//! `sync-stale`: no successful sync from an institution in N days (PRP §7,
//! built first). A dead aggregator connection is silent, and a monitoring
//! tool that stops monitoring without saying so is worse than no tool.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

pub struct SyncStale;

impl Rule for SyncStale {
    fn id(&self) -> &'static str {
        "sync-stale"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let threshold_days = i64::from(ctx.thresholds.sync_stale_days);
        ctx.health
            .iter()
            .filter_map(|institution| {
                let days_stale = institution
                    .last_successful_sync_at
                    .map(|last| (ctx.now - last).num_days());
                let is_stale = match days_stale {
                    Some(days) => days >= threshold_days,
                    // Never synced successfully: that is exactly the silence
                    // this rule exists to surface.
                    None => true,
                };
                if !is_stale {
                    return None;
                }
                // Episode identity: keyed by the last-success date, so an
                // ongoing outage alerts once, and a new outage alerts anew.
                let episode = institution
                    .last_successful_sync_at
                    .map(|last| last.to_rfc3339())
                    .unwrap_or_else(|| "never".to_string());
                let summary = match days_stale {
                    Some(days) => format!(
                        "No successful sync from {} in {} days (threshold {}).",
                        institution.institution, days, threshold_days
                    ),
                    None => format!(
                        "No successful sync from {} has ever been observed.",
                        institution.institution
                    ),
                };
                Some(Finding {
                    rule_id: self.id().to_string(),
                    severity: Severity::Alert,
                    subject: format!("institution:{}", institution.institution),
                    summary,
                    evidence: json!({
                        "institution": institution.institution,
                        "last_successful_sync_at": institution
                            .last_successful_sync_at
                            .map(|dt| dt.to_rfc3339()),
                        "days_stale": days_stale,
                        "threshold_days": threshold_days,
                    }),
                    dedupe_key: format!("sync-stale:{}:{}", institution.institution, episode),
                    detected_at: ctx.now,
                })
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};

    use super::*;
    use crate::config::RuleThresholds;
    use crate::store::repo::StoredInstitutionHealth;

    fn ctx_with_health(health: Vec<StoredInstitutionHealth>) -> EvalContext {
        EvalContext {
            now: Utc
                .with_ymd_and_hms(2026, 8, 18, 22, 0, 0)
                .single()
                .expect("ts"),
            thresholds: RuleThresholds::default(), // sync_stale_days = 4
            health,
            transactions: Vec::new(),
            balances: Default::default(),
            account_names: Default::default(),
            account_currencies: Default::default(),
            account_kinds: Default::default(),
        }
    }

    fn inst(name: &str, last: Option<DateTime<Utc>>) -> StoredInstitutionHealth {
        StoredInstitutionHealth {
            institution: name.into(),
            last_successful_sync_at: last,
            fetched_at: Utc
                .with_ymd_and_hms(2026, 8, 18, 21, 0, 0)
                .single()
                .expect("ts"),
        }
    }

    #[test]
    fn fires_when_last_success_is_older_than_threshold() {
        let last = Utc.with_ymd_and_hms(2026, 8, 13, 6, 0, 0).single(); // 5.6 days ago
        let findings = SyncStale.evaluate(&ctx_with_health(vec![inst("First National", last)]));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.rule_id, "sync-stale");
        assert_eq!(f.severity, Severity::Alert);
        assert!(f.subject.contains("First National"));
        assert_eq!(f.evidence["days_stale"], 5);
        assert_eq!(f.evidence["threshold_days"], 4);
        // Episode identity: the dedupe key is stable while the outage lasts,
        // and precise enough that two outages on one date stay distinct.
        assert_eq!(
            f.dedupe_key,
            "sync-stale:First National:2026-08-13T06:00:00+00:00"
        );
    }

    #[test]
    fn does_not_fire_within_threshold() {
        let last = Utc.with_ymd_and_hms(2026, 8, 16, 6, 0, 0).single(); // 2.7 days ago
        let findings = SyncStale.evaluate(&ctx_with_health(vec![inst("First National", last)]));
        assert!(findings.is_empty());
    }

    #[test]
    fn fires_for_an_institution_that_never_synced() {
        let findings = SyncStale.evaluate(&ctx_with_health(vec![inst("Ghost Bank", None)]));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Alert);
        assert_eq!(findings[0].dedupe_key, "sync-stale:Ghost Bank:never");
        assert_eq!(findings[0].evidence["days_stale"], serde_json::Value::Null);
    }

    #[test]
    fn threshold_comes_from_config_not_code() {
        let last = Utc.with_ymd_and_hms(2026, 8, 13, 6, 0, 0).single(); // 5.6 days ago
        let mut ctx = ctx_with_health(vec![inst("First National", last)]);
        ctx.thresholds.sync_stale_days = 10;
        assert!(SyncStale.evaluate(&ctx).is_empty());
    }

    #[test]
    fn healthy_and_stale_institutions_are_judged_independently() {
        // The store delivers health ordered by institution; the rule judges
        // each independently and preserves that order (global sorting is the
        // engine's job).
        let stale = Utc.with_ymd_and_hms(2026, 8, 1, 6, 0, 0).single();
        let fresh = Utc.with_ymd_and_hms(2026, 8, 18, 6, 0, 0).single();
        let findings = SyncStale.evaluate(&ctx_with_health(vec![
            inst("Alpha Bank", fresh),
            inst("Mid Bank", stale),
            inst("Zeta Credit Union", stale),
        ]));
        let subjects: Vec<&str> = findings.iter().map(|f| f.subject.as_str()).collect();
        assert_eq!(
            subjects,
            ["institution:Mid Bank", "institution:Zeta Credit Union"],
            "only the stale ones fire"
        );
    }
    #[test]
    fn two_outages_on_the_same_date_have_distinct_episode_keys() {
        let morning = Utc.with_ymd_and_hms(2026, 8, 1, 1, 0, 0).single();
        let evening = Utc.with_ymd_and_hms(2026, 8, 1, 20, 0, 0).single();
        let a = SyncStale.evaluate(&ctx_with_health(vec![inst("Bank", morning)]));
        let b = SyncStale.evaluate(&ctx_with_health(vec![inst("Bank", evening)]));
        assert_ne!(
            a[0].dedupe_key, b[0].dedupe_key,
            "distinct outages must not collapse into one finding"
        );
    }
}
