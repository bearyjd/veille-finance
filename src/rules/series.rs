//! Shared monthly-series qualifier: groups transactions by counterparty and
//! recognizes activity that has landed in each of the last
//! `recurring_min_occurrences` consecutive calendar months. Consuming rules
//! layer their own regularity, recency, and due-date judgment on top of the
//! representatives this module selects.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Datelike;

use super::EvalContext;
use crate::domain::Transaction;

/// A counterparty that has behaved like a monthly series: activity in each of
/// the last `min_occurrences` consecutive calendar months ending at its most
/// recent occurrence, one representative per month (closest to the streak
/// median amount, later date breaking ties). Regularity is NOT judged here —
/// each rule applies its own regularity test to the representative subset it
/// cares about.
pub(crate) struct MonthlySeries<'a> {
    pub counterparty: &'a str,
    /// Ascending by month; one per streak month.
    pub representatives: Vec<&'a Transaction>,
    /// (year, month) of the most recent occurrence.
    pub last_month: (i32, u32),
    /// Most recent occurrence overall (for typical-amount display).
    pub last: &'a Transaction,
}

/// Every counterparty (identified by `counterparty_key`) that qualifies as a
/// monthly series, sorted by counterparty. Empty when
/// `ctx.thresholds.recurring_min_occurrences` is 0.
pub(crate) fn monthly_series(ctx: &EvalContext) -> Vec<MonthlySeries<'_>> {
    let min_occurrences = ctx.thresholds.recurring_min_occurrences as usize;
    if min_occurrences == 0 {
        return Vec::new();
    }

    // Transfers count: a bill paid by internal transfer is still paid, and a
    // missed automated savings transfer is a missed deposit.
    let mut by_counterparty: BTreeMap<&str, Vec<&Transaction>> = BTreeMap::new();
    for t in &ctx.transactions {
        if let Some(key) = t.counterparty_key.as_deref() {
            by_counterparty.entry(key).or_default().push(t);
        }
    }

    let mut series = Vec::new();
    for (counterparty, occurrences) in by_counterparty {
        let Some(&last) = occurrences.last() else {
            continue;
        };
        let last_month = (last.posted_at.year(), last.posted_at.month());

        // Streak: each of the last `min_occurrences` calendar months up to
        // and including the last occurrence's month has activity.
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

        // One representative per streak month, so an unrelated extra charge
        // at the same counterparty cannot displace a month from the
        // statistics: the representative is the occurrence closest in
        // amount to the streak-wide median (later date breaks ties).
        let streak_months: BTreeSet<(i32, u32)> = {
            let mut set = BTreeSet::new();
            let mut m = last_month;
            for _ in 0..min_occurrences {
                set.insert(m);
                m = prev_month(m);
            }
            set
        };
        let in_streak: Vec<&Transaction> = occurrences
            .iter()
            .copied()
            .filter(|t| streak_months.contains(&(t.posted_at.year(), t.posted_at.month())))
            .collect();
        let global_median = median_u128(
            in_streak
                .iter()
                .map(|t| u128::from(t.amount_minor.unsigned_abs()))
                .collect(),
        );
        let representatives: Vec<&Transaction> = streak_months
            .iter()
            .filter_map(|month| {
                in_streak
                    .iter()
                    .copied()
                    .filter(|t| (t.posted_at.year(), t.posted_at.month()) == *month)
                    .min_by_key(|t| {
                        let amount = u128::from(t.amount_minor.unsigned_abs());
                        let distance = amount.abs_diff(global_median);
                        (distance, std::cmp::Reverse(t.posted_at))
                    })
            })
            .collect();

        series.push(MonthlySeries {
            counterparty,
            representatives,
            last_month,
            last,
        });
    }
    series
}

/// Max ≤ 1.5 × min over the given amounts (absolute minor units); false when
/// empty or any amount is zero.
pub(crate) fn amounts_regular(amounts: &[u128]) -> bool {
    let (Some(&min_amount), Some(&max_amount)) = (amounts.iter().min(), amounts.iter().max())
    else {
        return false;
    };
    min_amount != 0 && max_amount * 2 <= min_amount * 3
}

/// Median of sorted day-of-month values: middle element for odd counts, the
/// midpoint of the two middle elements for even counts.
pub(crate) fn median_day(sorted_days: &[u32]) -> u32 {
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

pub(crate) fn median_u128(mut values: Vec<u128>) -> u128 {
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

pub(crate) fn next_month((year, month): (i32, u32)) -> (i32, u32) {
    if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    }
}

pub(crate) fn prev_month((year, month): (i32, u32)) -> (i32, u32) {
    if month == 1 {
        (year - 1, 12)
    } else {
        (year, month - 1)
    }
}

pub(crate) fn format_month((year, month): (i32, u32)) -> String {
    format!("{year:04}-{month:02}")
}

/// The expected day, clamped into the month (day 31 in February becomes the
/// month's last day).
pub(crate) fn clamp_to_month((year, month): (i32, u32), day: u32) -> chrono::NaiveDate {
    (1..=day.max(1))
        .rev()
        .find_map(|d| chrono::NaiveDate::from_ymd_opt(year, month, d))
        .unwrap_or(chrono::NaiveDate::MIN)
}
