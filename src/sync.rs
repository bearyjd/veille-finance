//! The sync engine: pull from a [`SureSource`], materialize into the store.
//! Idempotent — running it twice against the same upstream state must not
//! create duplicate transaction rows (append-only snapshot tables gain one
//! row per entity per run by design). All writes for one run commit in a
//! single database transaction, so a failed run leaves no partial state.

use chrono::{DateTime, Duration, Utc};

use crate::domain::UpstreamHealth;
use crate::source::{SourceError, SureSource};
use crate::store::repo::UpsertStats;
use crate::store::{Store, StoreError, TenantId};

/// Default incremental window: how far behind the newest stored transaction
/// each sync starts, so late-arriving upstream backfills are still picked up.
/// Backfills older than the configured window are NOT caught by incremental
/// runs — schedule an occasional `--full` resync for that.
pub const DEFAULT_LOOKBACK_DAYS: u32 = 90;

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub lookback_days: u32,
    /// Ignore the watermark and pull full history.
    pub full: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            lookback_days: DEFAULT_LOOKBACK_DAYS,
            full: false,
        }
    }
}

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
/// them atomically. `now` is injected so runs are deterministic under test.
pub async fn sync_tenant(
    store: &Store,
    tenant: TenantId,
    source: &dyn SureSource,
    now: DateTime<Utc>,
    options: SyncOptions,
) -> Result<SyncOutcome, SyncError> {
    let since = if options.full {
        DateTime::<Utc>::MIN_UTC
    } else {
        match store.max_posted_at(tenant).await? {
            // checked: a pathological stored date must degrade to a full
            // fetch, not panic the process.
            Some(newest) => newest
                .checked_sub_signed(Duration::days(i64::from(options.lookback_days)))
                .and_then(|start| start.and_hms_opt(0, 0, 0))
                .map(|dt| dt.and_utc())
                .unwrap_or(DateTime::<Utc>::MIN_UTC),
            None => DateTime::<Utc>::MIN_UTC,
        }
    };

    let accounts = source.accounts().await?;
    let transactions = source.transactions(since).await?;
    let holdings = source.holdings().await?;
    let health = source.health().await?;

    let stats = store
        .materialize(tenant, &accounts, &transactions, &holdings, now)
        .await?;

    tracing::info!(
        tenant = ?tenant,
        accounts = stats.account_snapshots,
        tx_inserted = stats.transactions.inserted,
        tx_refreshed = stats.transactions.refreshed,
        tx_skipped_stale = stats.transactions.skipped_stale,
        holdings = stats.holding_snapshots,
        "sync complete"
    );

    Ok(SyncOutcome {
        account_snapshots: stats.account_snapshots,
        transactions: stats.transactions,
        holding_snapshots: stats.holding_snapshots,
        health,
    })
}
