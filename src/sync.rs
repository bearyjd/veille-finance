//! The sync engine: pull from a [`SureSource`], materialize into the store.
//! Idempotent — running it twice against the same upstream state must not
//! create duplicate transaction rows (append-only snapshot tables gain one
//! row per entity per run by design).

use chrono::{DateTime, Duration, Utc};

use crate::domain::UpstreamHealth;
use crate::source::{SourceError, SureSource};
use crate::store::repo::UpsertStats;
use crate::store::{Store, StoreError, TenantId};

/// How far behind the newest stored transaction each incremental sync starts,
/// so late-arriving upstream backfills are still picked up.
const LOOKBACK_DAYS: i64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug)]
pub struct SyncOutcome {
    pub account_snapshots: u64,
    pub transactions: UpsertStats,
    pub holding_snapshots: u64,
    pub health: UpstreamHealth,
}

/// Pull accounts, transactions, and holdings for one tenant and materialize
/// them. `now` is injected so runs are deterministic under test.
pub async fn sync_tenant(
    store: &Store,
    tenant: TenantId,
    source: &dyn SureSource,
    now: DateTime<Utc>,
) -> Result<SyncOutcome, SyncError> {
    let since = match store.max_posted_at(tenant).await? {
        Some(newest) => {
            let start = newest - Duration::days(LOOKBACK_DAYS);
            start
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc())
                .unwrap_or(DateTime::<Utc>::MIN_UTC)
        }
        None => DateTime::<Utc>::MIN_UTC,
    };

    let accounts = source.accounts().await?;
    let transactions = source.transactions(since).await?;
    let holdings = source.holdings().await?;
    let health = source.health().await?;

    let account_snapshots = store
        .insert_account_snapshots(tenant, &accounts, now)
        .await?;
    let tx_stats = store
        .upsert_transactions(tenant, &transactions, now)
        .await?;
    let holding_snapshots = store
        .insert_holding_snapshots(tenant, &holdings, now)
        .await?;

    tracing::info!(
        tenant = tenant_debug(tenant),
        accounts = account_snapshots,
        tx_inserted = tx_stats.inserted,
        tx_refreshed = tx_stats.refreshed,
        holdings = holding_snapshots,
        "sync complete"
    );

    Ok(SyncOutcome {
        account_snapshots,
        transactions: tx_stats,
        holding_snapshots,
        health,
    })
}

fn tenant_debug(tenant: TenantId) -> String {
    format!("{tenant:?}")
}
