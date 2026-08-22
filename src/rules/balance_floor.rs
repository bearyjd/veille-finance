//! `balance-floor`: a cash account's latest balance is below an absolute,
//! per-tenant floor (market survey: Copilot, Rocket Money). Complements
//! `balance-band` (statistical): the band asks "is this unusual for this
//! account?", the floor asks "is this simply too low, full stop?" — the
//! imminent-overdraft class of finding, so it is an Alert.
//!
//! Liability accounts are exempt: an account whose typical observed balance
//! is negative (credit cards, lines of credit) lives below any cash floor by
//! nature. The exemption keys on the median of the history, not "ever
//! negative" — a checking account that once overdrafted is exactly the
//! account this rule must keep watching.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity, format_minor};

pub struct BalanceFloor;

impl Rule for BalanceFloor {
    fn id(&self) -> &'static str {
        "balance-floor"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let Some(floor) = ctx.thresholds.balance_floor_minor else {
            return Vec::new();
        };
        let mut findings = Vec::new();
        for (account_id, series) in &ctx.balances {
            let Some(latest) = series.last() else {
                continue;
            };
            if median_minor(series.iter().map(|p| p.balance_minor)) < 0 {
                continue;
            }
            if latest.balance_minor >= floor {
                continue;
            }
            let currency = ctx.account_currency(account_id);
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Alert,
                subject: format!("account:{}", ctx.account_name(account_id)),
                summary: format!(
                    "{} balance is {}, below the {} floor.",
                    ctx.account_name(account_id),
                    format_minor(latest.balance_minor, currency),
                    format_minor(floor, currency),
                ),
                evidence: json!({
                    "account": account_id,
                    "balance_minor": latest.balance_minor,
                    "floor_minor": floor,
                    "currency": currency,
                    "as_of": latest.date.to_string(),
                }),
                dedupe_key: format!("balance-floor:{account_id}"),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

/// Median balance of the observed history; 0 for an empty series.
fn median_minor(values: impl Iterator<Item = i64>) -> i64 {
    let mut sorted: Vec<i64> = values.collect();
    if sorted.is_empty() {
        return 0;
    }
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        sorted[n / 2 - 1].midpoint(sorted[n / 2])
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::BalancePoint;
    use crate::rules::test_support::empty_ctx;

    fn ctx_with_series(account: &str, points: &[(&str, i64)]) -> super::super::EvalContext {
        let mut ctx = empty_ctx();
        ctx.thresholds.balance_floor_minor = Some(50_000);
        ctx.balances.insert(
            account.to_string(),
            points
                .iter()
                .map(|(d, b)| BalancePoint {
                    date: d.parse().expect("date"),
                    balance_minor: *b,
                })
                .collect(),
        );
        ctx.account_names
            .insert(account.to_string(), "Everyday Checking".to_string());
        ctx.account_currencies
            .insert(account.to_string(), "USD".to_string());
        ctx
    }

    #[test]
    fn fires_below_the_floor() {
        let ctx = ctx_with_series("a1", &[("2026-08-16", 90_000), ("2026-08-17", 30_000)]);
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Alert);
        assert_eq!(f.dedupe_key, "balance-floor:a1");
        assert_eq!(f.evidence["floor_minor"], 50_000);
    }

    #[test]
    fn silent_at_or_above_the_floor() {
        let ctx = ctx_with_series("a1", &[("2026-08-17", 50_000)]);
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn none_disables_the_rule() {
        let mut ctx = ctx_with_series("a1", &[("2026-08-17", 1_000)]);
        ctx.thresholds.balance_floor_minor = None;
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn liability_accounts_are_exempt() {
        // Median history negative: a credit card, not a cash account.
        let ctx = ctx_with_series(
            "card",
            &[
                ("2026-08-14", -120_000),
                ("2026-08-15", -90_000),
                ("2026-08-16", -100_000),
            ],
        );
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn an_overdrafted_checking_account_still_fires() {
        // Median positive, latest negative: this is the emergency, not an
        // exemption.
        let ctx = ctx_with_series(
            "a1",
            &[
                ("2026-08-14", 200_000),
                ("2026-08-15", 150_000),
                ("2026-08-16", -4_000),
            ],
        );
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["balance_minor"], -4_000);
    }

    #[test]
    fn dedupe_key_is_stable_across_days() {
        let day_one = ctx_with_series("a1", &[("2026-08-16", 30_000)]);
        let day_two = ctx_with_series("a1", &[("2026-08-16", 30_000), ("2026-08-17", 20_000)]);
        assert_eq!(
            BalanceFloor.evaluate(&day_one)[0].dedupe_key,
            BalanceFloor.evaluate(&day_two)[0].dedupe_key,
            "a continuing breach is one episode, not one finding per day"
        );
    }
}
