//! `duplicate-charge`: the same outflow amount hit the same account from the
//! same counterparty twice within a small window — a double-billed charge or
//! double-submitted payment (market survey: Carefull, Rocket Money). Exact
//! amount match on purpose: near-misses are ordinary repeat spending.
//! Transfers and inflows are not charges; amounts under the floor are noise.

use std::collections::BTreeMap;

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity, Transaction};

pub struct DuplicateCharge;

impl Rule for DuplicateCharge {
    fn id(&self) -> &'static str {
        "duplicate-charge"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let window = i64::from(ctx.thresholds.duplicate_window_days);
        let floor = ctx.thresholds.duplicate_floor_minor;

        let mut clusters: BTreeMap<(&str, &str, i64), Vec<&Transaction>> = BTreeMap::new();
        for t in ctx.recent_transactions() {
            if t.is_transfer || t.amount_minor >= 0 || t.amount_minor.saturating_neg() < floor {
                continue;
            }
            let Some(key) = t.counterparty_key.as_deref() else {
                continue;
            };
            clusters
                .entry((t.account_external_id.as_str(), key, t.amount_minor))
                .or_default()
                .push(t);
        }

        let mut findings = Vec::new();
        for ((account, counterparty, amount_minor), txns) in clusters {
            // Context transactions are ascending (posted_at, external_id), so
            // adjacent pairs are the closest-in-time candidates.
            let Some(pair) = txns
                .windows(2)
                .find(|p| (p[1].posted_at - p[0].posted_at).num_days() <= window)
            else {
                continue;
            };
            let (first, second) = (pair[0], pair[1]);
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Warn,
                subject: format!("account:{}", ctx.account_name(account)),
                summary: format!(
                    "\u{2018}{}\u{2019} charged {} twice within {} day(s): {} and {}.",
                    counterparty,
                    crate::domain::format_minor(amount_minor, &first.currency),
                    window,
                    first.posted_at,
                    second.posted_at,
                ),
                evidence: json!({
                    "account": account,
                    "counterparty": counterparty,
                    "amount_minor": amount_minor,
                    "currency": first.currency,
                    "occurrences": txns.iter().map(|t| t.posted_at.to_string()).collect::<Vec<_>>(),
                    "window_days": window,
                }),
                dedupe_key: format!(
                    "duplicate-charge:{account}:{counterparty}:{amount_minor}:{}",
                    first.posted_at
                ),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::{ctx_with_transactions, txn};

    // `now` in tests is 2026-08-18; the recent window reaches back to Jul 14.

    #[test]
    fn fires_on_identical_charges_close_together() {
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-11", -12_999, "Streaming Co"),
        ];
        let findings = DuplicateCharge.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(
            f.dedupe_key,
            "duplicate-charge:a1:streaming co:-12999:2026-08-10"
        );
        assert_eq!(f.evidence["occurrences"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn does_not_fire_outside_the_window() {
        let txns = vec![
            txn("d1", "a1", "2026-08-01", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-10", -12_999, "Streaming Co"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn different_amounts_are_not_duplicates() {
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-11", -13_999, "Streaming Co"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn small_amounts_stay_quiet() {
        // Two identical coffees under the $25.00 default floor.
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -650, "Corner Cafe"),
            txn("d2", "a1", "2026-08-10", -650, "Corner Cafe"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn inflows_and_transfers_are_not_charges() {
        let mut transfer_a = txn("t1", "a1", "2026-08-10", -50_000, "To Savings");
        transfer_a.is_transfer = true;
        let mut transfer_b = txn("t2", "a1", "2026-08-11", -50_000, "To Savings");
        transfer_b.is_transfer = true;
        let txns = vec![
            txn("i1", "a1", "2026-08-10", 250_000, "Acme Payroll"),
            txn("i2", "a1", "2026-08-11", 250_000, "Acme Payroll"),
            transfer_a,
            transfer_b,
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn separate_accounts_do_not_pair() {
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d2", "a2", "2026-08-10", -12_999, "Streaming Co"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn three_occurrences_emit_one_finding_keyed_on_the_first() {
        let txns = vec![
            txn("d1", "a1", "2026-08-09", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d3", "a1", "2026-08-11", -12_999, "Streaming Co"),
        ];
        let findings = DuplicateCharge.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].dedupe_key,
            "duplicate-charge:a1:streaming co:-12999:2026-08-09"
        );
        assert_eq!(
            findings[0].evidence["occurrences"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn window_comes_from_config() {
        let mut ctx = ctx_with_transactions(vec![
            txn("d1", "a1", "2026-08-01", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-10", -12_999, "Streaming Co"),
        ]);
        ctx.thresholds.duplicate_window_days = 10;
        assert_eq!(DuplicateCharge.evaluate(&ctx).len(), 1);
    }
}
