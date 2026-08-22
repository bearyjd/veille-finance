//! `recurring-price-change`: an established monthly series charged
//! noticeably more this month than its prior usual amount (market survey:
//! Rocket Money, Copilot). Regularity is judged over the PRIOR months only,
//! so a large jump cannot disqualify its own series. Increases only — a
//! price drop is good news, and good news is not a finding. Info severity:
//! digest-visible, never pushed; escalate only if real noise levels warrant.

use serde_json::json;

use super::series::{amounts_regular, format_month, median_u128, monthly_series};
use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

/// Increases below this many minor units never fire regardless of
/// percentage: a $0.60 bump on a $2.99 service is not worth a line.
pub const MIN_INCREASE_MINOR: u128 = 500;

pub struct RecurringPriceChange;

impl Rule for RecurringPriceChange {
    fn id(&self) -> &'static str {
        "recurring-price-change"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        use chrono::Datelike;

        let pct = u128::from(ctx.thresholds.recurring_price_increase_pct);
        if pct == 0 {
            return Vec::new();
        }
        let today = ctx.now.date_naive();
        let current_month = (today.year(), today.month());

        let mut findings = Vec::new();
        for series in monthly_series(ctx) {
            // Only a charge in the evaluation month is "the new price".
            if series.last_month != current_month {
                continue;
            }
            let (prior, newest): (Vec<_>, Vec<_>) = series
                .representatives
                .into_iter()
                .partition(|t| (t.posted_at.year(), t.posted_at.month()) != current_month);
            let Some(newest) = newest.first() else {
                continue;
            };
            let prior_amounts: Vec<u128> = prior
                .into_iter()
                .map(|t| u128::from(t.amount_minor.unsigned_abs()))
                .collect();
            if prior_amounts.is_empty() || !amounts_regular(&prior_amounts) {
                continue;
            }
            let usual = median_u128(prior_amounts);
            let new_amount = u128::from(newest.amount_minor.unsigned_abs());
            if new_amount <= usual
                || new_amount - usual < MIN_INCREASE_MINOR
                || new_amount * 100 <= usual * (100 + pct)
            {
                continue;
            }
            let increase_pct = (new_amount - usual) * 100 / usual;
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Info,
                subject: format!("counterparty:{}", series.counterparty),
                summary: format!(
                    "\u{2018}{}\u{2019} charged {} this month, up about {}% from its usual {}.",
                    series.counterparty,
                    crate::domain::format_minor(newest.amount_minor, &newest.currency),
                    increase_pct,
                    crate::domain::format_minor(
                        i64::try_from(usual).unwrap_or(i64::MAX) * newest.amount_minor.signum(),
                        &newest.currency
                    ),
                ),
                evidence: json!({
                    "counterparty": series.counterparty,
                    "month": format_month(current_month),
                    "new_amount_minor": newest.amount_minor,
                    "usual_amount_minor": usual.to_string(),
                    "increase_pct": increase_pct.to_string(),
                    "currency": newest.currency,
                }),
                dedupe_key: format!(
                    "recurring-price-change:{}:{}",
                    series.counterparty,
                    format_month(current_month)
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

    // `now` in tests is 2026-08-18.

    fn subscription(new_amount: i64) -> Vec<crate::domain::Transaction> {
        vec![
            txn("s1", "a1", "2026-05-12", -9_99, "Streaming Co"),
            txn("s2", "a1", "2026-06-12", -9_99, "Streaming Co"),
            txn("s3", "a1", "2026-07-12", -9_99, "Streaming Co"),
            txn("s4", "a1", "2026-08-12", new_amount, "Streaming Co"),
        ]
    }

    #[test]
    fn fires_on_a_clear_price_increase() {
        // $9.99 → $15.99: +60%, over the 20% default and the $5 min delta.
        let findings = RecurringPriceChange.evaluate(&ctx_with_transactions(subscription(-15_99)));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(f.dedupe_key, "recurring-price-change:streaming co:2026-08");
    }

    #[test]
    fn a_jump_beyond_regularity_still_fires() {
        // Prior months are regular; the doubled charge itself must not
        // disqualify the series (regularity judged on prior months only).
        let findings = RecurringPriceChange.evaluate(&ctx_with_transactions(subscription(-29_99)));
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn small_percentage_stays_quiet() {
        // $9.99 → $10.99: +10%, under the 20% default.
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(subscription(-10_99)))
                .is_empty()
        );
    }

    #[test]
    fn tiny_absolute_increase_stays_quiet() {
        // Prior $2.99, new $3.99: +33% but under MIN_INCREASE_MINOR.
        let txns = vec![
            txn("s1", "a1", "2026-05-12", -2_99, "Tiny App"),
            txn("s2", "a1", "2026-06-12", -2_99, "Tiny App"),
            txn("s3", "a1", "2026-07-12", -2_99, "Tiny App"),
            txn("s4", "a1", "2026-08-12", -3_99, "Tiny App"),
        ];
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn decreases_never_fire() {
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(subscription(-4_99)))
                .is_empty()
        );
    }

    #[test]
    fn no_current_month_charge_means_no_judgment() {
        // (That situation belongs to recurring-missing, not this rule.)
        let txns = vec![
            txn("s1", "a1", "2026-05-12", -9_99, "Streaming Co"),
            txn("s2", "a1", "2026-06-12", -9_99, "Streaming Co"),
            txn("s3", "a1", "2026-07-12", -9_99, "Streaming Co"),
        ];
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn irregular_prior_months_are_not_a_series() {
        let txns = vec![
            txn("s1", "a1", "2026-05-12", -2_00, "Corner Store"),
            txn("s2", "a1", "2026-06-12", -9_00, "Corner Store"),
            txn("s3", "a1", "2026-07-12", -30_00, "Corner Store"),
            txn("s4", "a1", "2026-08-12", -50_00, "Corner Store"),
        ];
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn threshold_pct_comes_from_config() {
        let mut ctx = ctx_with_transactions(subscription(-15_99));
        ctx.thresholds.recurring_price_increase_pct = 80;
        assert!(RecurringPriceChange.evaluate(&ctx).is_empty());
    }
}
