//! Tenant-scoped repositories. Every method takes a non-optional [`TenantId`]
//! and every SQL statement filters on it.
//!
//! Writes for one sync run go through [`Store::materialize`], which commits
//! everything in a single database transaction — a failed run leaves no
//! partial state to duplicate on retry.

use chrono::{DateTime, Utc};
use sqlx::SqliteConnection;

use super::{Result, Store, StoreError, TenantId};
use crate::domain::{Account, Holding, Transaction, UpstreamHealth};

/// One append-only account snapshot observation, for baseline building.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSnapshotRow {
    pub external_id: String,
    pub name: String,
    pub balance_minor: i64,
    pub currency: String,
    pub as_of_date: chrono::NaiveDate,
    pub kind: String,
}

/// One persisted finding, as the digest reads it back.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredFinding {
    pub rule_id: String,
    pub severity: crate::domain::Severity,
    pub subject: String,
    pub summary: String,
    pub evidence: serde_json::Value,
    pub detected_at: DateTime<Utc>,
    /// Refreshed each time the same condition is re-detected; drives the
    /// digest's "active during the period" selection.
    pub last_seen_at: DateTime<Utc>,
    pub dedupe_key: String,
}

/// One delivery audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryRow {
    pub channel: String,
    pub recipient: String,
    pub finding_keys: Vec<String>,
    pub sent_at: DateTime<Utc>,
}

/// One institution's health as recorded at the last sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredInstitutionHealth {
    pub institution: String,
    pub last_successful_sync_at: Option<DateTime<Utc>>,
    pub fetched_at: DateTime<Utc>,
}

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
        health: &UpstreamHealth,
        now: DateTime<Utc>,
    ) -> Result<MaterializeStats> {
        let mut tx = self.pool().begin().await?;
        let account_snapshots = insert_account_snapshots_on(&mut tx, tenant, accounts, now).await?;
        let tx_stats = upsert_transactions_on(&mut tx, tenant, transactions, now).await?;
        let holding_snapshots = insert_holding_snapshots_on(&mut tx, tenant, holdings, now).await?;
        replace_upstream_health_on(&mut tx, tenant, health).await?;
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

    /// Insert findings, ignoring any whose `(tenant_id, dedupe_key)` already
    /// exists — the same condition never re-alerts. Re-detection refreshes
    /// `last_seen_at` (forward only), which keeps an ongoing condition
    /// visible in every digest until it clears. Returns how many were new.
    pub async fn upsert_findings(
        &self,
        tenant: TenantId,
        findings: &[crate::domain::Finding],
    ) -> Result<u64> {
        let mut tx = self.pool().begin().await?;
        let mut new_rows = 0u64;
        for f in findings {
            let severity = f.severity.as_str();
            let evidence = f.evidence.to_string();
            let detected_at = f.detected_at.to_rfc3339();
            let inserted = sqlx::query!(
                "INSERT OR IGNORE INTO finding \
                 (tenant_id, rule_id, severity, subject, summary, evidence, detected_at, \
                  last_seen_at, dedupe_key) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                tenant.0,
                f.rule_id,
                severity,
                f.subject,
                f.summary,
                evidence,
                detected_at,
                detected_at,
                f.dedupe_key,
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if inserted == 1 {
                new_rows += 1;
            } else {
                sqlx::query!(
                    "UPDATE finding SET last_seen_at = ? \
                     WHERE tenant_id = ? AND dedupe_key = ? AND last_seen_at < ?",
                    detected_at,
                    tenant.0,
                    f.dedupe_key,
                    detected_at,
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(new_rows)
    }

    /// Findings ACTIVE in `[since, until]` — detected by `until` and still
    /// being re-detected at or after `since` — excluding acknowledged ones.
    /// An unresolved condition therefore appears in every digest until it
    /// clears. Ordered most severe first, then rule id, then dedupe key.
    pub async fn findings_active_in(
        &self,
        tenant: TenantId,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<StoredFinding>> {
        let since = since.to_rfc3339();
        let until = until.to_rfc3339();
        let rows = sqlx::query!(
            "SELECT rule_id, severity, subject, summary, evidence, detected_at, \
             last_seen_at, dedupe_key \
             FROM finding \
             WHERE tenant_id = ? AND last_seen_at >= ? AND detected_at <= ? \
             AND acknowledged_at IS NULL \
             ORDER BY detected_at, dedupe_key",
            tenant.0,
            since,
            until,
        )
        .fetch_all(self.pool())
        .await?;

        let mut findings: Vec<StoredFinding> = rows
            .into_iter()
            .map(|r| {
                Ok(StoredFinding {
                    rule_id: r.rule_id,
                    severity: r
                        .severity
                        .parse()
                        .map_err(|e: String| corrupt("finding.severity", &e))?,
                    subject: r.subject,
                    summary: r.summary,
                    evidence: serde_json::from_str(&r.evidence)
                        .map_err(|e| corrupt("finding.evidence", &e))?,
                    detected_at: DateTime::parse_from_rfc3339(&r.detected_at)
                        .map_err(|e| corrupt("finding.detected_at", &e))?
                        .with_timezone(&Utc),
                    last_seen_at: DateTime::parse_from_rfc3339(&r.last_seen_at)
                        .map_err(|e| corrupt("finding.last_seen_at", &e))?
                        .with_timezone(&Utc),
                    dedupe_key: r.dedupe_key,
                })
            })
            .collect::<Result<_>>()?;
        findings.sort_by(|a, b| {
            (std::cmp::Reverse(a.severity), &a.rule_id, &a.dedupe_key).cmp(&(
                std::cmp::Reverse(b.severity),
                &b.rule_id,
                &b.dedupe_key,
            ))
        });
        Ok(findings)
    }

    /// Alert-severity findings still owed a push: never pushed, not
    /// acknowledged. Failed pushes stay here and retry on the next run.
    pub async fn unpushed_alerts(&self, tenant: TenantId) -> Result<Vec<StoredFinding>> {
        let rows = sqlx::query!(
            "SELECT rule_id, severity, subject, summary, evidence, detected_at, \
             last_seen_at, dedupe_key \
             FROM finding \
             WHERE tenant_id = ? AND severity = 'alert' AND pushed_at IS NULL \
             AND acknowledged_at IS NULL \
             ORDER BY detected_at, dedupe_key",
            tenant.0
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(StoredFinding {
                    rule_id: r.rule_id,
                    severity: r
                        .severity
                        .parse()
                        .map_err(|e: String| corrupt("finding.severity", &e))?,
                    subject: r.subject,
                    summary: r.summary,
                    evidence: serde_json::from_str(&r.evidence)
                        .map_err(|e| corrupt("finding.evidence", &e))?,
                    detected_at: DateTime::parse_from_rfc3339(&r.detected_at)
                        .map_err(|e| corrupt("finding.detected_at", &e))?
                        .with_timezone(&Utc),
                    last_seen_at: DateTime::parse_from_rfc3339(&r.last_seen_at)
                        .map_err(|e| corrupt("finding.last_seen_at", &e))?
                        .with_timezone(&Utc),
                    dedupe_key: r.dedupe_key,
                })
            })
            .collect()
    }

    /// Atomically claim a pushed alert and write its audit row in one
    /// transaction. Returns `false` when another run already claimed it —
    /// in that case nothing is written. Either both the `pushed_at` mark and
    /// the audit row commit, or neither does: a partial state can neither
    /// silence an alert's audit trail nor starve its retry.
    pub async fn record_push(
        &self,
        tenant: TenantId,
        dedupe_key: &str,
        destination: &str,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        let at = at.to_rfc3339();
        let mut tx = self.pool().begin().await?;
        let claimed = sqlx::query!(
            "UPDATE finding SET pushed_at = ? \
             WHERE tenant_id = ? AND dedupe_key = ? AND pushed_at IS NULL",
            at,
            tenant.0,
            dedupe_key,
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if claimed == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        let finding_ids = serde_json::to_string(&[dedupe_key]).unwrap_or_else(|_| "[]".to_string());
        sqlx::query!(
            "INSERT INTO delivery (tenant_id, channel, recipient, finding_ids, sent_at) \
             VALUES (?, 'push', ?, ?, ?)",
            tenant.0,
            destination,
            finding_ids,
            at,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Record one digest send: one audit row per recipient, all in a single
    /// transaction — either every owner and watcher row commits together or
    /// none does (PRP §2.2: a partial write must never record a watcher
    /// without the owners).
    pub async fn record_smtp_deliveries(
        &self,
        tenant: TenantId,
        recipients: &[String],
        finding_keys: &[String],
        sent_at: DateTime<Utc>,
    ) -> Result<()> {
        let finding_ids =
            serde_json::to_string(finding_keys).map_err(|e| corrupt("delivery.finding_ids", &e))?;
        let sent_at = sent_at.to_rfc3339();
        let mut tx = self.pool().begin().await?;
        for recipient in recipients {
            sqlx::query!(
                "INSERT INTO delivery (tenant_id, channel, recipient, finding_ids, sent_at) \
                 VALUES (?, 'smtp', ?, ?, ?)",
                tenant.0,
                recipient,
                finding_ids,
                sent_at,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn deliveries(&self, tenant: TenantId) -> Result<Vec<DeliveryRow>> {
        let rows = sqlx::query!(
            "SELECT channel, recipient, finding_ids, sent_at \
             FROM delivery WHERE tenant_id = ? ORDER BY sent_at, channel, recipient",
            tenant.0
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(DeliveryRow {
                    channel: r.channel,
                    recipient: r.recipient,
                    finding_keys: serde_json::from_str(&r.finding_ids)
                        .map_err(|e| corrupt("delivery.finding_ids", &e))?,
                    sent_at: DateTime::parse_from_rfc3339(&r.sent_at)
                        .map_err(|e| corrupt("delivery.sent_at", &e))?
                        .with_timezone(&Utc),
                })
            })
            .collect()
    }

    /// When the most recent SMTP digest went out, if ever — the next digest's
    /// coverage window extends back to it so no findings can fall in a gap.
    pub async fn last_smtp_delivery_at(&self, tenant: TenantId) -> Result<Option<DateTime<Utc>>> {
        let row = sqlx::query_scalar!(
            r#"SELECT MAX(sent_at) as "d?: String" FROM delivery              WHERE tenant_id = ? AND channel = 'smtp'"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        row.map(|d| {
            DateTime::parse_from_rfc3339(&d)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| corrupt("delivery.sent_at", &e))
        })
        .transpose()
    }

    /// Claim and audit a batch of pushed alerts in one transaction (used for
    /// the capped-volume summary push covering many findings at once).
    /// Returns the dedupe keys actually claimed by this run.
    pub async fn record_push_batch(
        &self,
        tenant: TenantId,
        dedupe_keys: &[String],
        destination: &str,
        at: DateTime<Utc>,
    ) -> Result<Vec<String>> {
        let at = at.to_rfc3339();
        let mut tx = self.pool().begin().await?;
        let mut claimed = Vec::new();
        for key in dedupe_keys {
            let rows = sqlx::query!(
                "UPDATE finding SET pushed_at = ?                  WHERE tenant_id = ? AND dedupe_key = ? AND pushed_at IS NULL",
                at,
                tenant.0,
                key,
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if rows == 1 {
                claimed.push(key.clone());
            }
        }
        if claimed.is_empty() {
            tx.rollback().await?;
            return Ok(claimed);
        }
        let finding_ids =
            serde_json::to_string(&claimed).map_err(|e| corrupt("delivery.finding_ids", &e))?;
        sqlx::query!(
            "INSERT INTO delivery (tenant_id, channel, recipient, finding_ids, sent_at)              VALUES (?, 'push', ?, ?, ?)",
            tenant.0,
            destination,
            finding_ids,
            at,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(claimed)
    }

    /// Whether an SMTP digest already went out for this tenant on this
    /// calendar day (UTC) — a timer double-fire must not double-send.
    pub async fn smtp_delivered_on(
        &self,
        tenant: TenantId,
        day: chrono::NaiveDate,
    ) -> Result<bool> {
        let prefix = day.to_string();
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "c: i64" FROM delivery              WHERE tenant_id = ? AND channel = 'smtp' AND substr(sent_at, 1, 10) = ?"#,
            tenant.0,
            prefix,
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count > 0)
    }

    pub async fn finding_count(&self, tenant: TenantId) -> Result<u64> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "c: i64" FROM finding WHERE tenant_id = ?"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count as u64)
    }

    pub async fn delivery_count(&self, tenant: TenantId) -> Result<u64> {
        let count = sqlx::query_scalar!(
            r#"SELECT COUNT(*) as "c: i64" FROM delivery WHERE tenant_id = ?"#,
            tenant.0
        )
        .fetch_one(self.pool())
        .await?;
        Ok(count as u64)
    }

    /// Account snapshot observations up to `up_to`, ascending by observation
    /// time. The bound is pushed into SQL: the table is append-only and grows
    /// forever, so callers must not scan past their evaluation instant.
    pub async fn account_snapshot_rows(
        &self,
        tenant: TenantId,
        up_to: DateTime<Utc>,
    ) -> Result<Vec<AccountSnapshotRow>> {
        let up_to = up_to.to_rfc3339();
        let rows = sqlx::query!(
            "SELECT external_id, name, balance_minor, currency, as_of, kind \
             FROM account_snapshot WHERE tenant_id = ? AND as_of <= ? \
             ORDER BY as_of, external_id, id",
            tenant.0,
            up_to,
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(AccountSnapshotRow {
                    external_id: r.external_id,
                    name: r.name,
                    balance_minor: r.balance_minor,
                    currency: r.currency,
                    as_of_date: DateTime::parse_from_rfc3339(&r.as_of)
                        .map_err(|e| corrupt("account_snapshot.as_of", &e))?
                        .with_timezone(&Utc)
                        .date_naive(),
                    kind: r.kind,
                })
            })
            .collect()
    }

    /// Current per-institution upstream health as of the last sync.
    pub async fn upstream_health(&self, tenant: TenantId) -> Result<Vec<StoredInstitutionHealth>> {
        let rows = sqlx::query!(
            "SELECT institution, last_successful_sync_at, fetched_at \
             FROM upstream_health WHERE tenant_id = ? ORDER BY institution",
            tenant.0
        )
        .fetch_all(self.pool())
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(StoredInstitutionHealth {
                    institution: r.institution,
                    last_successful_sync_at: r
                        .last_successful_sync_at
                        .map(|v| {
                            DateTime::parse_from_rfc3339(&v)
                                .map(|dt| dt.with_timezone(&Utc))
                                .map_err(|e| corrupt("upstream_health.last_successful_sync_at", &e))
                        })
                        .transpose()?,
                    fetched_at: DateTime::parse_from_rfc3339(&r.fetched_at)
                        .map_err(|e| corrupt("upstream_health.fetched_at", &e))?
                        .with_timezone(&Utc),
                })
            })
            .collect()
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

/// Current-state semantics: the health table always reflects the latest sync,
/// so institutions that disappear upstream disappear here too.
async fn replace_upstream_health_on(
    conn: &mut SqliteConnection,
    tenant: TenantId,
    health: &UpstreamHealth,
) -> Result<()> {
    sqlx::query!("DELETE FROM upstream_health WHERE tenant_id = ?", tenant.0)
        .execute(&mut *conn)
        .await?;
    let fetched_at = health.fetched_at.to_rfc3339();
    for institution in &health.institutions {
        let last = institution
            .last_successful_sync_at
            .map(|dt| dt.to_rfc3339());
        sqlx::query!(
            "INSERT INTO upstream_health \
             (tenant_id, institution, last_successful_sync_at, fetched_at) \
             VALUES (?, ?, ?, ?)",
            tenant.0,
            institution.institution,
            last,
            fetched_at,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// A stored value failed to parse back into its domain type — the database
/// content itself is bad, which must surface loudly, never as a skipped row.
fn corrupt(column: &str, err: &dyn std::fmt::Display) -> StoreError {
    StoreError::Db(sqlx::Error::Decode(
        format!("corrupt value in {column}: {err}").into(),
    ))
}
