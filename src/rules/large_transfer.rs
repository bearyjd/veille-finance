//! `large-transfer`: a single transaction above a per-tenant absolute
//! threshold (PRP §7). Disabled until the operator sets the threshold —
//! there is no honest cross-household default.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

pub struct LargeTransfer;

impl Rule for LargeTransfer {
    fn id(&self) -> &'static str {
        "large-transfer"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let Some(threshold) = ctx.thresholds.large_transfer_minor else {
            return Vec::new();
        };
        let threshold = threshold.unsigned_abs();
        ctx.recent_transactions()
            // Matched transfers between the tenant's own accounts are routine
            // money movement; an outflow to an external destination is not
            // flagged as a transfer and is exactly what this rule watches.
            .filter(|t| !t.is_transfer)
            .filter(|t| t.amount_minor.unsigned_abs() >= threshold)
            .map(|t| Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Alert,
                subject: format!("account:{}", ctx.account_name(&t.account_external_id)),
                evidence: json!({
                    "amount_minor": t.amount_minor,
                    "currency": t.currency,
                    "description": t.description,
                    "account_external_id": t.account_external_id,
                    "posted_at": t.posted_at.to_string(),
                    "threshold_minor": ctx.thresholds.large_transfer_minor,
                }),
                dedupe_key: format!("large-transfer:{}", t.external_id),
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
    fn disabled_without_a_configured_threshold() {
        let ctx = ctx_with_transactions(vec![txn("t1", "a1", "2026-08-10", -9_999_999, "Wire")]);
        assert!(
            LargeTransfer.evaluate(&ctx).is_empty(),
            "no threshold configured means the rule is off"
        );
    }

    #[test]
    fn fires_at_or_above_threshold_in_either_direction() {
        let mut ctx = ctx_with_transactions(vec![
            txn("t-out", "a1", "2026-08-10", -500_000, "Wire out"),
            txn("t-in", "a1", "2026-08-11", 600_000, "Wire in"),
            txn("t-small", "a1", "2026-08-12", -499_999, "Under"),
        ]);
        ctx.thresholds.large_transfer_minor = Some(500_000);
        let findings = LargeTransfer.evaluate(&ctx);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|f| f.severity == Severity::Alert));
        let keys: Vec<&str> = findings.iter().map(|f| f.dedupe_key.as_str()).collect();
        assert!(keys.contains(&"large-transfer:t-out"));
        assert!(keys.contains(&"large-transfer:t-in"));
    }

    #[test]
    fn skips_internal_transfers_and_old_transactions() {
        let mut internal = txn("t-int", "a1", "2026-08-10", -900_000, "To savings");
        internal.is_transfer = true;
        let old = txn("t-old", "a1", "2025-01-01", -900_000, "Ancient wire");
        let mut ctx = ctx_with_transactions(vec![internal, old]);
        ctx.thresholds.large_transfer_minor = Some(500_000);
        assert!(
            LargeTransfer.evaluate(&ctx).is_empty(),
            "internal transfers are routine; backfilled history must not flood"
        );
    }
}
