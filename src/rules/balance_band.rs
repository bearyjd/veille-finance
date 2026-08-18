//! `balance-band`: an account balance moves outside a rolling band derived
//! from its own history (PRP §7). `Info` outside the band, `Warn` beyond 2×.
//!
//! Band formula (integer math throughout — statistics, but derived from and
//! compared against integer minor units):
//!   band = max(2σ, |mean| / 20, MIN_BAND_MINOR)
//! computed over the last `balance_band_window_days` of daily observations
//! strictly before the newest one. Fewer than [`MIN_BASELINE_POINTS`] prior
//! observations means no judgment — new accounts stay quiet while a baseline
//! accumulates.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

/// Below this many baseline observations the rule stays silent.
pub const MIN_BASELINE_POINTS: usize = 5;
/// The band never narrows below this, so a perfectly flat balance does not
/// alert on trivial movement. (Minor units — $100 for 2-exponent currencies.)
pub const MIN_BAND_MINOR: i64 = 10_000;

pub struct BalanceBand;

impl Rule for BalanceBand {
    fn id(&self) -> &'static str {
        "balance-band"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (account_id, series) in &ctx.balances {
            let Some((latest, history)) = series.split_last() else {
                continue;
            };
            let window_start = latest.date
                - chrono::Duration::days(i64::from(ctx.thresholds.balance_band_window_days));
            let baseline: Vec<i128> = history
                .iter()
                .filter(|p| p.date >= window_start)
                .map(|p| i128::from(p.balance_minor))
                .collect();
            if baseline.len() < MIN_BASELINE_POINTS {
                continue;
            }

            // Integer statistics: truncating division is fine at band scale.
            // Saturating arithmetic: balances near i64 extremes must degrade
            // to a maximally wide band, never wrap or panic.
            let n = baseline.len() as i128;
            let mean = baseline.iter().sum::<i128>() / n;
            let variance = baseline
                .iter()
                .map(|b| {
                    let d = b - mean;
                    d.checked_mul(d).unwrap_or(i128::MAX)
                })
                .fold(0i128, i128::saturating_add)
                / n;
            let sigma = (variance.unsigned_abs()).isqrt() as i128;
            let band = sigma
                .saturating_mul(2)
                .max(mean.abs() / 20)
                .max(i128::from(MIN_BAND_MINOR));

            let deviation = i128::from(latest.balance_minor) - mean;
            if deviation.abs() <= band {
                continue;
            }
            let severity = if deviation.abs() > band.saturating_mul(2) {
                Severity::Warn
            } else {
                Severity::Info
            };
            let direction = if deviation > 0 { "above" } else { "below" };
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity,
                subject: format!("account:{}", ctx.account_name(account_id)),
                summary: format!(
                    "{} balance {} is {} its typical range (about {}).",
                    ctx.account_name(account_id),
                    crate::domain::format_minor(
                        latest.balance_minor,
                        ctx.account_currency(account_id)
                    ),
                    direction,
                    crate::domain::format_minor(clamp_i64(mean), ctx.account_currency(account_id)),
                ),
                evidence: json!({
                    "account_external_id": account_id,
                    "balance_minor": latest.balance_minor,
                    "mean_minor": clamp_i64(mean),
                    "band_minor": clamp_i64(band),
                    "deviation_minor": clamp_i64(deviation),
                    "direction": direction,
                    "window_days": ctx.thresholds.balance_band_window_days,
                    "baseline_points": baseline.len(),
                }),
                dedupe_key: format!("balance-band:{account_id}:{direction}"),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

/// Evidence values narrow to i64: clamp instead of wrapping, so a saturated
/// statistic can never flip sign in the record.
fn clamp_i64(value: i128) -> i64 {
    i64::try_from(value).unwrap_or(if value > 0 { i64::MAX } else { i64::MIN })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::BalancePoint;
    use crate::rules::test_support::empty_ctx;

    fn ctx_with_series(points: Vec<(&str, i64)>) -> crate::rules::EvalContext {
        let mut ctx = empty_ctx();
        let series: Vec<BalancePoint> = points
            .into_iter()
            .map(|(d, b)| BalancePoint {
                date: d.parse().expect("date"),
                balance_minor: b,
            })
            .collect();
        ctx.balances.insert("a1".into(), series);
        ctx.account_names.insert("a1".into(), "Checking".into());
        ctx
    }

    fn flat_history() -> Vec<(&'static str, i64)> {
        vec![
            ("2026-08-01", 100_000),
            ("2026-08-02", 100_000),
            ("2026-08-03", 100_000),
            ("2026-08-04", 100_000),
            ("2026-08-05", 100_000),
            ("2026-08-06", 100_000),
        ]
    }

    #[test]
    fn stable_balance_does_not_fire() {
        let mut points = flat_history();
        points.push(("2026-08-17", 100_000));
        assert!(BalanceBand.evaluate(&ctx_with_series(points)).is_empty());
    }

    #[test]
    fn a_large_move_warns_and_a_moderate_move_informs() {
        // Flat 100_000 baseline: band = max(0, 5_000, 10_000) = 10_000.
        let mut big = flat_history();
        big.push(("2026-08-17", 200_000)); // deviation 100_000 > 2×band
        let findings = BalanceBand.evaluate(&ctx_with_series(big));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert_eq!(findings[0].dedupe_key, "balance-band:a1:above");
        assert_eq!(findings[0].evidence["band_minor"], 10_000);

        let mut moderate = flat_history();
        moderate.push(("2026-08-17", 112_000)); // deviation 12_000: outside band, inside 2×
        let findings = BalanceBand.evaluate(&ctx_with_series(moderate));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Info);

        let mut inside = flat_history();
        inside.push(("2026-08-17", 109_000)); // deviation 9_000: inside band
        assert!(BalanceBand.evaluate(&ctx_with_series(inside)).is_empty());
    }

    #[test]
    fn drops_fire_with_direction_below() {
        let mut points = flat_history();
        points.push(("2026-08-17", 20_000));
        let findings = BalanceBand.evaluate(&ctx_with_series(points));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].dedupe_key, "balance-band:a1:below");
    }

    #[test]
    fn too_little_history_stays_quiet() {
        let points = vec![
            ("2026-08-01", 100_000),
            ("2026-08-02", 100_000),
            ("2026-08-17", 900_000),
        ];
        assert!(
            BalanceBand.evaluate(&ctx_with_series(points)).is_empty(),
            "fewer than MIN_BASELINE_POINTS prior observations means no judgment"
        );
    }

    #[test]
    fn window_comes_from_config() {
        // Baseline points all older than a 10-day window: nothing to compare
        // against, so quiet.
        let points = vec![
            ("2026-05-01", 100_000),
            ("2026-05-02", 100_000),
            ("2026-05-03", 100_000),
            ("2026-05-04", 100_000),
            ("2026-05-05", 100_000),
            ("2026-05-06", 100_000),
            ("2026-08-17", 900_000),
        ];
        let mut ctx = ctx_with_series(points);
        ctx.thresholds.balance_band_window_days = 10;
        assert!(BalanceBand.evaluate(&ctx).is_empty());
    }
    #[test]
    fn extreme_balances_do_not_panic_or_corrupt_direction() {
        let points = vec![
            ("2026-08-01", i64::MAX),
            ("2026-08-02", i64::MAX),
            ("2026-08-03", i64::MAX),
            ("2026-08-04", i64::MIN),
            ("2026-08-05", i64::MIN),
            ("2026-08-17", i64::MAX),
        ];
        // Must not overflow in debug builds.
        let findings = BalanceBand.evaluate(&ctx_with_series(points));
        for f in &findings {
            let direction = f.evidence["direction"].as_str().expect("direction");
            let deviation = f.evidence["deviation_minor"].as_i64().expect("deviation");
            if direction == "above" {
                assert!(
                    deviation >= 0,
                    "direction and deviation sign must agree: {f:?}"
                );
            } else {
                assert!(
                    deviation <= 0,
                    "direction and deviation sign must agree: {f:?}"
                );
            }
        }
    }
}
