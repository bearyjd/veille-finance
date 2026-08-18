//! `dormant-card-wake`: an account with no activity for N days posts a
//! transaction (PRP §7). Catches a card that should be idle waking up.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

pub struct DormantCardWake;

impl Rule for DormantCardWake {
    fn id(&self) -> &'static str {
        "dormant-card-wake"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        use std::collections::BTreeMap;

        use crate::rules::RECENT_TRANSACTION_WINDOW_DAYS;

        let dormant_days = i64::from(ctx.thresholds.dormant_days);
        let cutoff = ctx.now.date_naive() - chrono::Duration::days(RECENT_TRANSACTION_WINDOW_DAYS);

        let mut last_activity: BTreeMap<&str, chrono::NaiveDate> = BTreeMap::new();
        let mut findings = Vec::new();
        // Transactions arrive ascending; every transaction (transfers
        // included — a transfer into a dormant account is still a wake)
        // both closes a gap and updates the activity marker.
        for t in &ctx.transactions {
            if let Some(previous) = last_activity.get(t.account_external_id.as_str()) {
                let gap_days = (t.posted_at - *previous).num_days();
                if gap_days >= dormant_days && t.posted_at >= cutoff {
                    findings.push(Finding {
                        rule_id: self.id().to_string(),
                        severity: Severity::Warn,
                        subject: format!("account:{}", ctx.account_name(&t.account_external_id)),
                        evidence: json!({
                            "account_external_id": t.account_external_id,
                            "gap_days": gap_days,
                            "dormant_days": dormant_days,
                            "amount_minor": t.amount_minor,
                            "currency": t.currency,
                            "description": t.description,
                            "posted_at": t.posted_at.to_string(),
                        }),
                        dedupe_key: format!(
                            "dormant-card-wake:{}:{}",
                            t.account_external_id, t.external_id
                        ),
                        detected_at: ctx.now,
                    });
                }
            }
            last_activity.insert(&t.account_external_id, t.posted_at);
        }
        findings
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::{ctx_with_transactions, txn};

    #[test]
    fn fires_when_a_dormant_account_wakes() {
        // Default dormancy is 60 days; gap here is ~7 months.
        let ctx = ctx_with_transactions(vec![
            txn("t0", "card-1", "2026-01-05", -1_500, "Old charge"),
            txn("t1", "card-1", "2026-08-10", -4_200, "Sudden charge"),
        ]);
        let findings = DormantCardWake.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.dedupe_key, "dormant-card-wake:card-1:t1");
        assert_eq!(f.evidence["gap_days"], 217);
    }

    #[test]
    fn active_accounts_do_not_fire() {
        let ctx = ctx_with_transactions(vec![
            txn("t0", "card-1", "2026-07-20", -1_500, "Charge"),
            txn("t1", "card-1", "2026-08-10", -4_200, "Charge"),
        ]);
        assert!(DormantCardWake.evaluate(&ctx).is_empty());
    }

    #[test]
    fn an_accounts_first_ever_transaction_is_not_a_wake() {
        let ctx = ctx_with_transactions(vec![txn("t1", "card-1", "2026-08-10", -4_200, "First")]);
        assert!(
            DormantCardWake.evaluate(&ctx).is_empty(),
            "no prior activity means no dormancy gap to measure"
        );
    }

    #[test]
    fn only_the_waking_transaction_fires_not_followups() {
        let ctx = ctx_with_transactions(vec![
            txn("t0", "card-1", "2026-01-05", -1_500, "Old charge"),
            txn("t1", "card-1", "2026-08-10", -4_200, "Wake"),
            txn("t2", "card-1", "2026-08-11", -2_000, "Follow-up"),
        ]);
        let findings = DormantCardWake.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].dedupe_key, "dormant-card-wake:card-1:t1");
    }

    #[test]
    fn dormancy_threshold_comes_from_config() {
        let ctx_txns = vec![
            txn("t0", "card-1", "2026-07-01", -1_500, "Charge"),
            txn("t1", "card-1", "2026-08-10", -4_200, "Charge"), // 40-day gap
        ];
        let mut ctx = ctx_with_transactions(ctx_txns);
        ctx.thresholds.dormant_days = 30;
        assert_eq!(DormantCardWake.evaluate(&ctx).len(), 1);
    }
}
