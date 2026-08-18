//! Append-only snapshot store (PRP §6). SQLite, single file, WAL mode.
//!
//! Tenant isolation is enforced at this layer: every repository method takes
//! a non-optional [`TenantId`], and every query is scoped by it. There is no
//! way to ask the store for "all tenants' " data.

pub mod repo;

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("unknown tenant slug: {0}")]
    UnknownTenant(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Opaque tenant handle. Obtainable only from [`repo::TenantRepo`], which is
/// what makes "forgot to scope by tenant" unrepresentable in repository code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TenantId(pub(crate) i64);

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if needed) the store at `path` and bring the schema up
    /// to date. On Unix the file is restricted to the owner — it holds every
    /// tenant's financial history.
    pub async fn open(path: &Path) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        // Before any data lands. SQLite's -wal/-shm sidecars inherit the main
        // file's permissions.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| StoreError::Db(sqlx::Error::Io(e)))?;
        }

        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
