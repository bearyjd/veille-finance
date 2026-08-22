//! `balance-floor`: a cash account's latest balance is below an absolute,
//! per-tenant floor (market survey: Copilot, Rocket Money). Complements
//! `balance-band` (statistical): the band asks "is this unusual for this
//! account?", the floor asks "is this simply too low, full stop?" — the
//! imminent-overdraft class of finding, so it is an Alert.
//!
//! Liability accounts are exempt, judged solely on the account's declared
//! kind (Sure vocabulary, stored verbatim: depository, credit_card,
//! investment, loan, crypto, property, vehicle, other_asset,
//! other_liability). A `depository` account is judged unconditionally —
//! regardless of its balance history's sign, since a chronically overdrawn
//! checking account is exactly what this rule must watch. Any other declared
//! kind is exempt: a card or loan below a cash floor is its normal state.
//! `account_kinds` is populated from the same snapshot rows as `balances` in
//! the same `build_context` pass, so every account with a balance series
//! also has a kind entry. A first observation is never judged, regardless
//! of kind.
//!
//! Episode identity: v1 has no clearing path (findings are never explicitly
//! resolved), so the dedupe key must carry the episode itself rather than
//! rely on it being cleared. The key is scoped to the current consecutive
//! below-floor run, identified by that run's first date — a continuing
//! breach stays one episode, and a recovery followed by a later re-breach is
//! a new one (mirrors `sync-stale`'s last-success discriminator).

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
            let Some((latest, history)) = series.split_last() else {
                continue;
            };
            if history.is_empty() {
                continue;
            }
            // Every account with a balance series also has a kind entry
            // (both maps are populated from the same snapshot rows in the
            // same build_context pass). Only a declared depository account
            // is judged — any other kind (credit_card, loan, investment,
            // ...) has below-floor as its normal state.
            if ctx.account_kinds.get(account_id).map(String::as_str) != Some("depository") {
                continue;
            }
            if latest.balance_minor >= floor {
                continue;
            }
            let currency = ctx.account_currency(account_id);
            // Episode identity: the first day of the current below-floor run. A
            // continuing breach stays one episode; recovery followed by a later
            // re-breach is a new one (same pattern as sync-stale's last-success
            // discriminator — v1 has no clearing path, so the key must carry it).
            let episode_start = series
                .iter()
                .rev()
                .take_while(|p| p.balance_minor < floor)
                .last()
                .map(|p| p.date)
                .unwrap_or(latest.date);
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
                dedupe_key: format!("balance-floor:{account_id}:{episode_start}"),
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
        ctx.account_kinds
            .insert(account.to_string(), "depository".to_string());
        ctx
    }

    #[test]
    fn fires_below_the_floor() {
        let ctx = ctx_with_series("a1", &[("2026-08-16", 90_000), ("2026-08-17", 30_000)]);
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Alert);
        assert_eq!(f.dedupe_key, "balance-floor:a1:2026-08-17");
        assert_eq!(f.evidence["floor_minor"], 50_000);
    }

    #[test]
    fn silent_at_or_above_the_floor() {
        let ctx = ctx_with_series("a1", &[("2026-08-16", 90_000), ("2026-08-17", 50_000)]);
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
        // Declared kind is credit_card, not depository: exempt regardless of
        // balance history.
        let mut ctx = ctx_with_series(
            "card",
            &[
                ("2026-08-14", -120_000),
                ("2026-08-15", -90_000),
                ("2026-08-16", -100_000),
            ],
        );
        ctx.account_kinds
            .insert("card".to_string(), "credit_card".to_string());
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn distressed_depository_still_judged() {
        // Declared kind is depository: judged regardless of a
        // majority-negative history — this is the inversion fix. Previously
        // the median heuristic would have read this as a liability account
        // and exempted it, but a chronically overdrawn checking account is
        // exactly what this rule must watch.
        let ctx = ctx_with_series("a1", &[("2026-08-14", -90_000), ("2026-08-15", -80_000)]);
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["balance_minor"], -80_000);
    }

    #[test]
    fn first_observation_is_never_judged() {
        // Single observation (no prior history) is never judged, whether positive or negative.
        let ctx_positive = ctx_with_series("a1", &[("2026-08-16", 1_000)]);
        assert!(BalanceFloor.evaluate(&ctx_positive).is_empty());

        let ctx_negative = ctx_with_series("a2", &[("2026-08-16", -120_000)]);
        assert!(BalanceFloor.evaluate(&ctx_negative).is_empty());
    }

    #[test]
    fn overdraft_after_one_healthy_day_fires() {
        // History median positive (50_000), latest negative: this is the emergency.
        let ctx = ctx_with_series("a1", &[("2026-08-15", 50_000), ("2026-08-16", -500_000)]);
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["balance_minor"], -500_000);
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
        let day_one = ctx_with_series("a1", &[("2026-08-15", 90_000), ("2026-08-16", 30_000)]);
        let day_two = ctx_with_series(
            "a1",
            &[
                ("2026-08-15", 90_000),
                ("2026-08-16", 30_000),
                ("2026-08-17", 20_000),
            ],
        );
        let key_one = BalanceFloor.evaluate(&day_one)[0].dedupe_key.clone();
        let key_two = BalanceFloor.evaluate(&day_two)[0].dedupe_key.clone();
        assert_eq!(
            key_one, key_two,
            "a continuing breach is one episode, not one finding per day"
        );
        // The run started 2026-08-16 in both cases (the day the balance first
        // dropped below the floor and never recovered).
        assert_eq!(key_one, "balance-floor:a1:2026-08-16");
    }

    #[test]
    fn recovery_then_rebreach_is_a_new_episode() {
        let first_breach = ctx_with_series("a1", &[("2026-08-10", 90_000), ("2026-08-11", 30_000)]);
        let second_breach = ctx_with_series(
            "a1",
            &[
                ("2026-08-10", 90_000),
                ("2026-08-11", 30_000),
                ("2026-08-12", 60_000),
                ("2026-08-13", 20_000),
            ],
        );
        let findings_first = BalanceFloor.evaluate(&first_breach);
        let findings_second = BalanceFloor.evaluate(&second_breach);
        assert_eq!(findings_first.len(), 1);
        assert_eq!(findings_second.len(), 1);
        assert_eq!(findings_first[0].dedupe_key, "balance-floor:a1:2026-08-11");
        assert_eq!(findings_second[0].dedupe_key, "balance-floor:a1:2026-08-13");
        assert_ne!(findings_first[0].dedupe_key, findings_second[0].dedupe_key);
    }
}
