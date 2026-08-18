//! The rule engine (PRP §7). Rules are deterministic pure functions of an
//! [`EvalContext`]; the engine does all store IO. A finding is emitted only
//! by rule code — nothing else creates, suppresses, escalates, or
//! de-escalates one (PRP §2.4).

pub mod balance_band;
pub mod dormant_card_wake;
pub mod large_transfer;
pub mod new_counterparty;
pub mod recurring_missing;
pub mod sync_stale;

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

use crate::config::RuleThresholds;
use crate::domain::{Finding, Transaction};
use crate::store::repo::StoredInstitutionHealth;
use crate::store::{Store, StoreError, TenantId};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// One balance observation for one account on one day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalancePoint {
    pub date: NaiveDate,
    pub balance_minor: i64,
}

/// Everything a rule may look at. Built once per tenant per evaluation;
/// rules never touch the store or the network.
#[derive(Debug)]
pub struct EvalContext {
    pub now: DateTime<Utc>,
    pub thresholds: RuleThresholds,
    pub health: Vec<StoredInstitutionHealth>,
    /// Full transaction history, ascending by (posted_at, external_id).
    pub transactions: Vec<Transaction>,
    /// Per-account balance series (last observation per day, ascending),
    /// keyed by account external id.
    pub balances: BTreeMap<String, Vec<BalancePoint>>,
    /// Account display names keyed by external id (latest snapshot wins).
    pub account_names: BTreeMap<String, String>,
}

pub trait Rule: Send + Sync {
    fn id(&self) -> &'static str;
    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding>;
}

/// Per-transaction rules only consider transactions posted within this many
/// days of `now`: a historical backfill must not flood findings for years-old
/// activity. Dedupe keys make re-evaluation quiet regardless.
pub const RECENT_TRANSACTION_WINDOW_DAYS: i64 = 35;

impl EvalContext {
    /// Transactions posted within [`RECENT_TRANSACTION_WINDOW_DAYS`] of `now`.
    pub fn recent_transactions(&self) -> impl Iterator<Item = &Transaction> {
        let cutoff = self.now.date_naive() - chrono::Duration::days(RECENT_TRANSACTION_WINDOW_DAYS);
        self.transactions
            .iter()
            .filter(move |t| t.posted_at >= cutoff)
    }

    pub fn account_name<'a>(&'a self, external_id: &'a str) -> &'a str {
        self.account_names
            .get(external_id)
            .map(String::as_str)
            .unwrap_or(external_id)
    }
}

/// The v1 rule set. Order is irrelevant; output is sorted.
pub fn all_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(sync_stale::SyncStale),
        Box::new(large_transfer::LargeTransfer),
        Box::new(new_counterparty::NewCounterparty),
        Box::new(dormant_card_wake::DormantCardWake),
        Box::new(balance_band::BalanceBand),
        Box::new(recurring_missing::RecurringMissing),
    ]
}

/// Run every rule and return findings sorted by (rule_id, dedupe_key) so
/// output is stable and diffable.
pub fn run_rules(ctx: &EvalContext) -> Vec<Finding> {
    let mut findings: Vec<Finding> = all_rules()
        .iter()
        .flat_map(|rule| {
            let out = rule.evaluate(ctx);
            debug_assert!(
                out.iter().all(|f| f.rule_id == rule.id()),
                "a rule may only emit findings under its own id"
            );
            out
        })
        .collect();
    findings.sort_by(|a, b| {
        (a.rule_id.as_str(), a.dedupe_key.as_str())
            .cmp(&(b.rule_id.as_str(), b.dedupe_key.as_str()))
    });
    findings
}

/// Evaluate one tenant: build the context, run every rule, and — unless this
/// is a dry run (`persist: false`) — record the findings. A dry run performs
/// no writes of any kind (PRP §10: `--dry-run` must be genuinely inert).
pub async fn evaluate_tenant(
    store: &Store,
    tenant: TenantId,
    thresholds: RuleThresholds,
    now: DateTime<Utc>,
    persist: bool,
) -> Result<Vec<Finding>, EngineError> {
    let ctx = build_context(store, tenant, thresholds, now).await?;
    let findings = run_rules(&ctx);
    if persist {
        let new_rows = store.upsert_findings(tenant, &findings).await?;
        tracing::info!(
            tenant = ?tenant,
            findings = findings.len(),
            new = new_rows,
            "evaluation recorded"
        );
    }
    Ok(findings)
}

/// Load everything the rules need for one tenant.
pub async fn build_context(
    store: &Store,
    tenant: TenantId,
    thresholds: RuleThresholds,
    now: DateTime<Utc>,
) -> Result<EvalContext, EngineError> {
    let health = store.upstream_health(tenant).await?;
    let transactions = store.list_transactions(tenant).await?;

    let mut balances: BTreeMap<String, Vec<BalancePoint>> = BTreeMap::new();
    let mut account_names: BTreeMap<String, String> = BTreeMap::new();
    for row in store.account_snapshot_rows(tenant).await? {
        let series = balances.entry(row.external_id.clone()).or_default();
        match series.last_mut() {
            // Several syncs in one day: the last observation of the day wins.
            Some(last) if last.date == row.as_of_date => last.balance_minor = row.balance_minor,
            _ => series.push(BalancePoint {
                date: row.as_of_date,
                balance_minor: row.balance_minor,
            }),
        }
        account_names.insert(row.external_id, row.name);
    }

    Ok(EvalContext {
        now,
        thresholds,
        health,
        transactions,
        balances,
        account_names,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
pub(crate) mod test_support {
    use chrono::{DateTime, TimeZone, Utc};

    use super::EvalContext;
    use crate::config::RuleThresholds;
    use crate::domain::Transaction;

    pub fn eval_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 18, 22, 0, 0)
            .single()
            .expect("ts")
    }

    pub fn empty_ctx() -> EvalContext {
        EvalContext {
            now: eval_now(),
            thresholds: RuleThresholds::default(),
            health: Vec::new(),
            transactions: Vec::new(),
            balances: Default::default(),
            account_names: Default::default(),
        }
    }

    /// `amount_minor` signed: positive = inflow.
    pub fn txn(id: &str, account: &str, date: &str, amount_minor: i64, name: &str) -> Transaction {
        Transaction {
            external_id: id.into(),
            account_external_id: account.into(),
            posted_at: date.parse().expect("date"),
            amount_minor,
            currency: "USD".into(),
            description: name.into(),
            category: None,
            counterparty_key: Some(name.to_lowercase()),
            is_transfer: false,
            updated_at: eval_now(),
        }
    }

    /// Context whose transactions are `txns` sorted ascending like the store
    /// delivers them.
    pub fn ctx_with_transactions(mut txns: Vec<Transaction>) -> EvalContext {
        txns.sort_by(|a, b| {
            (a.posted_at, a.external_id.clone()).cmp(&(b.posted_at, b.external_id.clone()))
        });
        EvalContext {
            transactions: txns,
            ..empty_ctx()
        }
    }
}
