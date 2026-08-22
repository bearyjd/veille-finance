//! `recurring-missing`: a transaction that recurs monthly (±window) did not
//! appear (PRP §7). Catches missed bills and missed deposits.
//!
//! A counterparty counts as a monthly series when it appears in each of the
//! last `recurring_min_occurrences` consecutive calendar months and its
//! amounts are regular (max ≤ 1.5 × min, by absolute value) — regularity is
//! what separates a bill or paycheck from variable spending at a familiar
//! merchant. The series is "missing" when the month after the streak has
//! passed its expected day (median day of month) plus `recurring_day_window`
//! days with no occurrence. Only the current and immediately previous months
//! are judged: a series that died long ago must not alert forever.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

pub struct RecurringMissing;

impl Rule for RecurringMissing {
    fn id(&self) -> &'static str {
        "recurring-missing"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        use std::collections::{BTreeMap, BTreeSet};

        use chrono::Datelike;

        use crate::domain::Transaction;

        let min_occurrences = ctx.thresholds.recurring_min_occurrences as usize;
        if min_occurrences == 0 {
            return Vec::new();
        }
        let day_window = i64::from(ctx.thresholds.recurring_day_window);
        let today = ctx.now.date_naive();
        let current_month = (today.year(), today.month());
        let previous_month = prev_month(current_month);

        // Transfers count: a bill paid by internal transfer is still paid,
        // and a missed automated savings transfer is a missed deposit.
        let mut by_counterparty: BTreeMap<&str, Vec<&Transaction>> = BTreeMap::new();
        for t in &ctx.transactions {
            if let Some(key) = t.counterparty_key.as_deref() {
                by_counterparty.entry(key).or_default().push(t);
            }
        }

        let mut findings = Vec::new();
        for (counterparty, occurrences) in by_counterparty {
            let Some(last) = occurrences.last() else {
                continue;
            };
            let last_month = (last.posted_at.year(), last.posted_at.month());

            // The month after the streak is the one owed a payment. Judge
            // only the current and immediately previous months: a series
            // that died long ago must not alert forever. This also handles
            // "the bill arrived": its month joins the streak and the
            // expected month moves into the future.
            let expected_month = next_month(last_month);
            if expected_month != current_month && expected_month != previous_month {
                continue;
            }

            // Streak: each of the last `min_occurrences` calendar months up
            // to and including the last occurrence's month has activity.
            let months: BTreeSet<(i32, u32)> = occurrences
                .iter()
                .map(|t| (t.posted_at.year(), t.posted_at.month()))
                .collect();
            let mut month = last_month;
            let streak_complete = (0..min_occurrences).all(|_| {
                let hit = months.contains(&month);
                month = prev_month(month);
                hit
            });
            if !streak_complete {
                continue;
            }

            // One representative per streak month, so an unrelated extra
            // charge at the same counterparty cannot displace a month from
            // the statistics: the representative is the occurrence closest
            // in amount to the streak-wide median (later date breaks ties).
            let streak_months: BTreeSet<(i32, u32)> = {
                let mut set = BTreeSet::new();
                let mut m = last_month;
                for _ in 0..min_occurrences {
                    set.insert(m);
                    m = prev_month(m);
                }
                set
            };
            let in_streak: Vec<&&Transaction> = occurrences
                .iter()
                .filter(|t| streak_months.contains(&(t.posted_at.year(), t.posted_at.month())))
                .collect();
            let global_median = median_u128(
                in_streak
                    .iter()
                    .map(|t| u128::from(t.amount_minor.unsigned_abs()))
                    .collect(),
            );
            let representatives: Vec<&&Transaction> = streak_months
                .iter()
                .filter_map(|month| {
                    in_streak
                        .iter()
                        .filter(|t| (t.posted_at.year(), t.posted_at.month()) == *month)
                        .min_by_key(|t| {
                            let amount = u128::from(t.amount_minor.unsigned_abs());
                            let distance = amount.abs_diff(global_median);
                            (distance, std::cmp::Reverse(t.posted_at))
                        })
                        .copied()
                })
                .collect();

            // Regularity separates a bill or paycheck from variable spending:
            // across the representatives, max ≤ 1.5 × min.
            let amounts: Vec<u128> = representatives
                .iter()
                .map(|t| u128::from(t.amount_minor.unsigned_abs()))
                .collect();
            let (Some(&min_amount), Some(&max_amount)) =
                (amounts.iter().min(), amounts.iter().max())
            else {
                continue;
            };
            if min_amount == 0 || max_amount * 2 > min_amount * 3 {
                continue;
            }

            // Direction decides severity: a missed bill reads fine in the
            // Sunday digest, but a missed deposit (pension, payroll, Social
            // Security) is the most time-critical signal this tool watches —
            // Alert, so the push channel carries it the same day.
            let is_inflow = representatives.iter().all(|t| t.amount_minor > 0);

            let mut days: Vec<u32> = representatives.iter().map(|t| t.posted_at.day()).collect();
            days.sort_unstable();
            let expected_day = median_day(&days);

            let due_by =
                clamp_to_month(expected_month, expected_day) + chrono::Duration::days(day_window);
            if today <= due_by {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: if is_inflow {
                    Severity::Alert
                } else {
                    Severity::Warn
                },
                subject: format!("counterparty:{counterparty}"),
                summary: format!(
                    "Expected monthly \u{2018}{}\u{2019} (about {} around day {}) has not appeared for {}.",
                    counterparty,
                    crate::domain::format_minor(last.amount_minor, &last.currency),
                    expected_day,
                    format_month(expected_month),
                ),
                evidence: json!({
                    "counterparty": counterparty,
                    "expected_month": format_month(expected_month),
                    "expected_day": expected_day,
                    "day_window": day_window,
                    "due_by": due_by.to_string(),
                    "last_seen": last.posted_at.to_string(),
                    "months_in_streak": min_occurrences,
                    "typical_amount_minor": last.amount_minor,
                    "currency": last.currency,
                    "direction": if is_inflow { "inflow" } else { "outflow" },
                }),
                dedupe_key: format!(
                    "recurring-missing:{counterparty}:{}",
                    format_month(expected_month)
                ),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

/// Median of sorted day-of-month values: middle element for odd counts, the
/// midpoint of the two middle elements for even counts.
fn median_day(sorted_days: &[u32]) -> u32 {
    let n = sorted_days.len();
    if n == 0 {
        return 1;
    }
    if n % 2 == 1 {
        sorted_days[n / 2]
    } else {
        (sorted_days[n / 2 - 1] + sorted_days[n / 2]) / 2
    }
}

fn median_u128(mut values: Vec<u128>) -> u128 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2
    }
}

fn next_month((year, month): (i32, u32)) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

fn prev_month((year, month): (i32, u32)) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

fn format_month((year, month): (i32, u32)) -> String {
    format!("{year:04}-{month:02}")
}

/// The expected day, clamped into the month (day 31 in February becomes the
/// month's last day).
fn clamp_to_month((year, month): (i32, u32), day: u32) -> chrono::NaiveDate {
    (1..=day.max(1))
        .rev()
        .find_map(|d| chrono::NaiveDate::from_ymd_opt(year, month, d))
        .unwrap_or(chrono::NaiveDate::MIN)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::{ctx_with_transactions, txn};

    // `now` in tests is 2026-08-18.

    fn utility_bill_series() -> Vec<crate::domain::Transaction> {
        vec![
            txn("m1", "a1", "2026-05-05", -9_000, "City Electric"),
            txn("m2", "a1", "2026-06-04", -9_500, "City Electric"),
            txn("m3", "a1", "2026-07-06", -9_200, "City Electric"),
        ]
    }

    #[test]
    fn fires_when_a_monthly_bill_misses_its_window() {
        // Median day 5 + window 5 = due by Aug 10; now is Aug 18, no August
        // occurrence.
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(utility_bill_series()));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.dedupe_key, "recurring-missing:city electric:2026-08");
        assert_eq!(f.evidence["expected_day"], 5);
    }

    #[test]
    fn does_not_fire_when_the_bill_arrived() {
        let mut txns = utility_bill_series();
        txns.push(txn("m4", "a1", "2026-08-06", -9_300, "City Electric"));
        assert!(
            RecurringMissing
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn does_not_fire_before_the_window_closes() {
        // Median day 25 + window 5 = due by Aug 30; now is Aug 18.
        let txns = vec![
            txn("m1", "a1", "2026-05-25", -9_000, "City Electric"),
            txn("m2", "a1", "2026-06-25", -9_500, "City Electric"),
            txn("m3", "a1", "2026-07-25", -9_200, "City Electric"),
        ];
        assert!(
            RecurringMissing
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn irregular_amounts_are_not_a_series() {
        let txns = vec![
            txn("m1", "a1", "2026-05-05", -2_000, "Corner Store"),
            txn("m2", "a1", "2026-06-04", -9_000, "Corner Store"),
            txn("m3", "a1", "2026-07-06", -30_000, "Corner Store"),
        ];
        assert!(
            RecurringMissing
                .evaluate(&ctx_with_transactions(txns))
                .is_empty(),
            "variable spending at a familiar merchant is not a bill"
        );
    }

    #[test]
    fn a_series_that_died_months_ago_stays_quiet() {
        let txns = vec![
            txn("m1", "a1", "2025-12-05", -9_000, "Old Gym"),
            txn("m2", "a1", "2026-01-05", -9_000, "Old Gym"),
            txn("m3", "a1", "2026-02-05", -9_000, "Old Gym"),
        ];
        assert!(
            RecurringMissing
                .evaluate(&ctx_with_transactions(txns))
                .is_empty(),
            "expected month 2026-03 is long past; cancelled subscriptions must not alert forever"
        );
    }

    #[test]
    fn missed_deposits_fire_as_alert() {
        let txns = vec![
            txn("p1", "a1", "2026-05-01", 250_000, "Acme Payroll"),
            txn("p2", "a1", "2026-06-01", 250_000, "Acme Payroll"),
            txn("p3", "a1", "2026-07-01", 250_000, "Acme Payroll"),
        ];
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].severity,
            Severity::Alert,
            "a missed deposit must push immediately, not wait for Sunday"
        );
        assert_eq!(findings[0].evidence["direction"], "inflow");
        assert_eq!(
            findings[0].dedupe_key,
            "recurring-missing:acme payroll:2026-08"
        );
    }

    #[test]
    fn mixed_direction_series_stays_warn() {
        // Regular |amounts|, alternating sign: not clearly income.
        let txns = vec![
            txn("m1", "a1", "2026-05-05", 9_000, "Odd Ledger"),
            txn("m2", "a1", "2026-06-04", -9_000, "Odd Ledger"),
            txn("m3", "a1", "2026-07-06", 9_000, "Odd Ledger"),
        ];
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert_eq!(findings[0].evidence["direction"], "outflow");
    }

    #[test]
    fn min_occurrences_comes_from_config() {
        let mut ctx = ctx_with_transactions(utility_bill_series());
        ctx.thresholds.recurring_min_occurrences = 4;
        assert!(RecurringMissing.evaluate(&ctx).is_empty());
    }

    #[test]
    fn non_consecutive_months_are_not_a_series() {
        let txns = vec![
            txn("m1", "a1", "2026-03-05", -9_000, "Quarterly Thing"),
            txn("m2", "a1", "2026-05-05", -9_000, "Quarterly Thing"),
            txn("m3", "a1", "2026-07-06", -9_200, "Quarterly Thing"),
        ];
        assert!(
            RecurringMissing
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }
    #[test]
    fn a_bill_paid_by_transfer_counts_as_present() {
        let mut txns = utility_bill_series();
        let mut august = txn("m4", "a1", "2026-08-05", -9_300, "City Electric");
        august.is_transfer = true;
        txns.push(august);
        assert!(
            RecurringMissing
                .evaluate(&ctx_with_transactions(txns))
                .is_empty(),
            "a transfer occurrence is still an occurrence"
        );
    }

    #[test]
    fn recurring_transfers_are_a_series_too() {
        let mk = |id: &str, date: &str| {
            let mut t = txn(id, "a1", date, -50_000, "To Savings");
            t.is_transfer = true;
            t
        };
        let txns = vec![
            mk("s1", "2026-05-01"),
            mk("s2", "2026-06-01"),
            mk("s3", "2026-07-01"),
        ];
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(txns));
        assert_eq!(
            findings.len(),
            1,
            "a missed automated savings transfer is a missed deposit"
        );
    }

    #[test]
    fn an_unrelated_extra_charge_does_not_hide_a_missing_bill() {
        let mut txns = utility_bill_series();
        // Same counterparty, one-off irregular amount late in July.
        txns.push(txn("x1", "a1", "2026-07-30", -100_000, "City Electric"));
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(txns));
        assert_eq!(
            findings.len(),
            1,
            "per-month representatives must be chosen, not the last N raw transactions"
        );
    }

    #[test]
    fn even_count_median_uses_the_middle_of_the_two() {
        let txns = vec![
            txn("m1", "a1", "2026-04-01", -9_000, "Split Bill"),
            txn("m2", "a1", "2026-05-02", -9_000, "Split Bill"),
            txn("m3", "a1", "2026-06-28", -9_000, "Split Bill"),
            txn("m4", "a1", "2026-07-29", -9_000, "Split Bill"),
        ];
        let mut ctx = ctx_with_transactions(txns);
        ctx.thresholds.recurring_min_occurrences = 4;
        // Due date is Aug 15 + 5; evaluate after it has passed.
        ctx.now = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 8, 21, 22, 0, 0)
            .single()
            .expect("ts");
        let findings = RecurringMissing.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].evidence["expected_day"], 15,
            "median of [1,2,28,29] is 15, not the upper middle"
        );
    }
}
