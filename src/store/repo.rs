//! Tenant-scoped repositories. Every method takes a non-optional [`TenantId`]
//! and every SQL statement filters on it.
//!
//! Writes for one sync run go through [`Store::materialize`], which commits
//! everything in a single database transaction — a failed run leaves no
//! partial state to duplicate on retry.

use chrono::{DateTime, Utc};
use sqlx::SqliteConnection;

use super::{Result, Store, StoreError, TenantId};
use crate::domain::{Account, Holding, Transaction};

/// Row counts reported by an idempotent transaction upsert. `refreshed` are
/// existing rows re-written from upstream; `skipped_stale` are incoming rows
/// older (by `updated_at`) than what is already stored, which are ignored.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UpsertStats {
    pub inserted: u64,
    pub refreshed: u64,
    pub skipped_stale: u64,
}

/// Counts from one atomic materialization run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MaterializeStats {
    pub account_snapshots: u64,
    pub transactions: UpsertStats,
    pub holding_snapshots: u64,
}

pub struct TenantRepo<'a> {
    store: &'a Store,
}

impl Store {
    pub fn tenants(&self) -> TenantRepo<'_> {
        TenantRepo { store: self }
    }
}

impl TenantRepo<'_> {
    /// Insert the tenant if its slug is new; either way return its id.
    pub async fn ensure(&self, slug: &str, display_name: &str) -> Result<TenantId> {
        sqlx::query!(
            "INSERT OR IGNORE INTO tenant (slug, display_name) VALUES (?, ?)",
            slug,
            display_name
        )
        .execute(self.store.pool())
        .await?;
        self.by_slug(slug).await
    }

    pub async fn by_slug(&self, slug: &str) -> Result<TenantId> {
        let row = sqlx::query!(
            r#"SELECT id as "id!: i64" FROM tenant WHERE slug = ?"#,
            slug
        )
        .fetch_optional(self.store.pool())
        .await?;
        row.map(|r| TenantId(r.id))
            .ok_or_else(|| StoreError::UnknownTenant(slug.to_string()))
    }
}

impl Store {
    /// Materialize one sync run atomically: account snapshots, transaction
    /// upserts, and holding snapshots commit together or not at all.
    pub async fn materialize(
        &self,
        tenant: TenantId,
        accounts: &[Account],
        transactions: &[Transaction],
        holdings: &[Holding],
        now: DateTime<Utc>,
    ) -> Result<MaterializeStats> {
        let mut tx = self.pool().begin().await?;
        let account_snapshots = insert_account_snapshots_on(&mut tx, tenant, accounts, now).await?;
        let tx_stats = upsert_transactions_on(&mut tx, tenant, transactions, now).await?;
        let holding_snapshots = insert_holding_snapshots_on(&mut tx, tenant, holdings, now).await?;
        tx.commit().await?;
        Ok(MaterializeStats {
            account_snapshots,
            transactions: tx_stats,
            holding_snapshots,
        })
    }

    /// Append one snapshot row per account (append-only table).
    pub async fn insert_account_snapshots(
        &self,
        tenant: TenantId,
        accounts: &[Account],
        as_of: DateTime<Utc>,
    ) -> Result<u64> {
        let mut tx = self.pool().begin().await?;
        let count = insert_account_snapshots_on(&mut tx, tenant, accounts, as_of).await?;
        tx.commit().await?;
        Ok(count)
    }

    /// Idempotent upsert on `(tenant_id, external_id)`. `first_seen_at` is set
    /// once on insert and never touched again; mutable fields are refreshed
    /// from upstream on conflict, unless the incoming row is older (by
    /// `updated_at`) than the stored one.
    pub async fn upsert_transactions(
        &self,
        tenant: TenantId,
        transactions: &[Transaction],
        now: DateTime<Utc>,
    ) -> Result<UpsertStats> {
        let mut tx = self.pool().begin().await?;
        let stats = upsert_transactions_on(&mut tx, tenant, transactions, now).await?;
        tx.commit().await?;
        Ok(stats)
    }

    /// Append one snapshot row per holding (append-only table).
    pub async fn insert_holding_snapshots(
        &self,
        tenant: TenantId,
        holdings: &[Holding],
        as_of: DateTime<Utc>,
    ) -> Result<u64> {
        let mut tx = self.pool().begin().await?;
        let count = insert_holding_snapshots_on(&mut tx, tenant, holdings, as_of).await?;
        tx.commit().await?;
        Ok(count)
    }

    /// Newest posted date stored for this tenant, or `None` when empty.
    pub async fn max_posted_at(&self, tenant: TenantId) -> Result<Option<chrono::NaiveDate>> {
        let row = sqlx::query_scalar!(
            r#"SELECT MAX(posted_at) as "d?: String" FROM transactions WHERE tenant_id = ?"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        row.map(|d| d.parse().map_err(|e| corrupt("transactions.posted_at", &e)))
            .transpose()
    }

    pub async fn transaction_count(&self, tenant: TenantId) -> Result<u64> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "c: i64" FROM transactions WHERE tenant_id = ?"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count as u64)
    }

    pub async fn list_transactions(&self, tenant: TenantId) -> Result<Vec<Transaction>> {
        let rows = sqlx::query!(
            "SELECT external_id, account_external_id, posted_at, amount_minor, currency, \
             description, category, counterparty_key, is_transfer, updated_at \
             FROM transactions WHERE tenant_id = ? ORDER BY posted_at, external_id",
            tenant.0
        )
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|r| {
                Ok(Transaction {
                    external_id: r.external_id,
                    account_external_id: r.account_external_id,
                    posted_at: r
                        .posted_at
                        .parse()
                        .map_err(|e| corrupt("transactions.posted_at", &e))?,
                    amount_minor: r.amount_minor,
                    currency: r.currency,
                    description: r.description,
                    category: r.category,
                    counterparty_key: r.counterparty_key,
                    is_transfer: r.is_transfer != 0,
                    updated_at: DateTime::parse_from_rfc3339(&r.updated_at)
                        .map_err(|e| corrupt("transactions.updated_at", &e))?
                        .with_timezone(&Utc),
                })
            })
            .collect()
    }

    pub async fn account_snapshot_count(&self, tenant: TenantId) -> Result<u64> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "c: i64" FROM account_snapshot WHERE tenant_id = ?"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count as u64)
    }

    pub async fn holding_snapshot_count(&self, tenant: TenantId) -> Result<u64> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "c: i64" FROM holding_snapshot WHERE tenant_id = ?"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count as u64)
    }
}

async fn insert_account_snapshots_on(
    conn: &mut SqliteConnection,
    tenant: TenantId,
    accounts: &[Account],
    as_of: DateTime<Utc>,
) -> Result<u64> {
    let as_of = as_of.to_rfc3339();
    for a in accounts {
        sqlx::query!(
            "INSERT INTO account_snapshot \
             (tenant_id, external_id, name, institution, kind, status, balance_minor, currency, as_of) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            tenant.0,
            a.external_id,
            a.name,
            a.institution,
            a.kind,
            a.status,
            a.balance_minor,
            a.currency,
            as_of,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(accounts.len() as u64)
}

async fn upsert_transactions_on(
    conn: &mut SqliteConnection,
    tenant: TenantId,
    transactions: &[Transaction],
    now: DateTime<Utc>,
) -> Result<UpsertStats> {
    let now = now.to_rfc3339();
    let mut stats = UpsertStats::default();
    for t in transactions {
        let posted_at = t.posted_at.to_string();
        let is_transfer = i64::from(t.is_transfer);
        let updated_at = t.updated_at.to_rfc3339();
        let inserted = sqlx::query!(
            "INSERT OR IGNORE INTO transactions \
             (tenant_id, external_id, account_external_id, posted_at, amount_minor, currency, \
              description, category, counterparty_key, is_transfer, first_seen_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            tenant.0,
            t.external_id,
            t.account_external_id,
            posted_at,
            t.amount_minor,
            t.currency,
            t.description,
            t.category,
            t.counterparty_key,
            is_transfer,
            now,
            updated_at,
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();

        if inserted == 1 {
            stats.inserted += 1;
            continue;
        }

        // Refresh only when the incoming row is at least as new as the stored
        // one — a stale duplicate (in-batch or from an older concurrent run)
        // must never overwrite newer data. RFC 3339 UTC strings compare
        // chronologically as text.
        let refreshed = sqlx::query!(
            "UPDATE transactions SET \
             account_external_id = ?, posted_at = ?, amount_minor = ?, currency = ?, \
             description = ?, category = ?, counterparty_key = ?, is_transfer = ?, updated_at = ? \
             WHERE tenant_id = ? AND external_id = ? AND updated_at <= ?",
            t.account_external_id,
            posted_at,
            t.amount_minor,
            t.currency,
            t.description,
            t.category,
            t.counterparty_key,
            is_transfer,
            updated_at,
            tenant.0,
            t.external_id,
            updated_at,
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();

        if refreshed == 1 {
            stats.refreshed += 1;
        } else {
            stats.skipped_stale += 1;
        }
    }
    Ok(stats)
}

async fn insert_holding_snapshots_on(
    conn: &mut SqliteConnection,
    tenant: TenantId,
    holdings: &[Holding],
    as_of: DateTime<Utc>,
) -> Result<u64> {
    let as_of = as_of.to_rfc3339();
    for h in holdings {
        let position_date = h.as_of_date.to_string();
        sqlx::query!(
            "INSERT INTO holding_snapshot \
             (tenant_id, account_external_id, symbol, quantity, market_value_minor, currency, \
              position_date, as_of) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            tenant.0,
            h.account_external_id,
            h.symbol,
            h.quantity,
            h.market_value_minor,
            h.currency,
            position_date,
            as_of,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(holdings.len() as u64)
}

/// A stored value failed to parse back into its domain type — the database
/// content itself is bad, which must surface loudly, never as a skipped row.
fn corrupt(column: &str, err: &dyn std::fmt::Display) -> StoreError {
    StoreError::Db(sqlx::Error::Decode(
        format!("corrupt value in {column}: {err}").into(),
    ))
}
