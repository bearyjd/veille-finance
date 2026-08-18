//! `new-counterparty`: a first-ever payee, above a floor amount (PRP §7).

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

pub struct NewCounterparty;

impl Rule for NewCounterparty {
    fn id(&self) -> &'static str {
        "new-counterparty"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        use std::collections::BTreeMap;

        use crate::domain::Transaction;
        use crate::rules::RECENT_TRANSACTION_WINDOW_DAYS;

        let floor = ctx.thresholds.new_counterparty_floor_minor.unsigned_abs();
        let cutoff = ctx.now.date_naive() - chrono::Duration::days(RECENT_TRANSACTION_WINDOW_DAYS);

        // First-ever occurrence per counterparty, over the FULL history —
        // transfers included: "new" means new to the whole record.
        let mut first_seen: BTreeMap<&str, &Transaction> = BTreeMap::new();
        for t in &ctx.transactions {
            let Some(key) = t.counterparty_key.as_deref() else {
                continue;
            };
            first_seen.entry(key).or_insert(t);
        }

        first_seen
            .into_iter()
            // A first appearance that is itself an internal transfer is
            // routine movement, not a new payee.
            .filter(|(_, t)| !t.is_transfer)
            .filter(|(_, t)| t.posted_at >= cutoff)
            .filter(|(_, t)| t.amount_minor.unsigned_abs() >= floor)
            .map(|(key, t)| Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Warn,
                subject: format!("counterparty:{key}"),
                summary: format!(
                    "First-ever payee \u{2018}{}\u{2019}: {} on {}.",
                    t.description,
                    crate::domain::format_minor(t.amount_minor, &t.currency),
                    t.posted_at,
                ),
                evidence: json!({
                    "counterparty": key,
                    "amount_minor": t.amount_minor,
                    "currency": t.currency,
                    "description": t.description,
                    "account_external_id": t.account_external_id,
                    "first_seen": t.posted_at.to_string(),
                    "floor_minor": ctx.thresholds.new_counterparty_floor_minor,
                }),
                dedupe_key: format!("new-counterparty:{key}"),
                detected_at: ctx.now,
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::{ctx_with_transactions, txn};

    #[test]
    fn fires_once_for_a_new_payee_above_the_floor() {
        // Default floor is 50_000 minor units.
        let ctx = ctx_with_transactions(vec![
            txn("t1", "a1", "2026-08-10", -60_000, "Roof Repair LLC"),
            txn("t2", "a1", "2026-08-12", -70_000, "Roof Repair LLC"),
        ]);
        let findings = NewCounterparty.evaluate(&ctx);
        assert_eq!(
            findings.len(),
            1,
            "one finding per counterparty, not per transaction"
        );
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.dedupe_key, "new-counterparty:roof repair llc");
        assert_eq!(
            f.evidence["amount_minor"], -60_000,
            "the first occurrence is the evidence"
        );
    }

    #[test]
    fn known_counterparties_do_not_fire() {
        // Payee seen long ago (outside the recent window): later charges are
        // not "new".
        let ctx = ctx_with_transactions(vec![
            txn("t0", "a1", "2025-01-05", -80_000, "Roof Repair LLC"),
            txn("t1", "a1", "2026-08-10", -90_000, "Roof Repair LLC"),
        ]);
        assert!(NewCounterparty.evaluate(&ctx).is_empty());
    }

    #[test]
    fn below_floor_and_transfers_and_keyless_do_not_fire() {
        let mut transfer = txn("t-tr", "a1", "2026-08-10", -900_000, "To savings");
        transfer.is_transfer = true;
        let mut keyless = txn("t-nk", "a1", "2026-08-11", -900_000, "???");
        keyless.counterparty_key = None;
        let ctx = ctx_with_transactions(vec![
            txn("t-small", "a1", "2026-08-09", -49_999, "Tiny Vendor"),
            transfer,
            keyless,
        ]);
        assert!(NewCounterparty.evaluate(&ctx).is_empty());
    }

    #[test]
    fn floor_comes_from_config() {
        let mut ctx = ctx_with_transactions(vec![txn("t1", "a1", "2026-08-10", -60_000, "Vendor")]);
        ctx.thresholds.new_counterparty_floor_minor = 100_000;
        assert!(NewCounterparty.evaluate(&ctx).is_empty());
    }
    #[test]
    fn a_counterparty_first_seen_via_transfer_is_not_new_later() {
        let mut old_transfer = txn("t0", "a1", "2026-01-10", -80_000, "Vendor Co");
        old_transfer.is_transfer = true;
        let ctx = ctx_with_transactions(vec![
            old_transfer,
            txn("t1", "a1", "2026-08-10", -90_000, "Vendor Co"),
        ]);
        assert!(
            NewCounterparty.evaluate(&ctx).is_empty(),
            "first-ever means first in the FULL history, transfers included"
        );
    }
}
