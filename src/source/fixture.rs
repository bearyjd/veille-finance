//! Golden-file implementation of [`SureSource`]: reads a directory of JSON
//! files in Sure's `/api/v1` wire format (`accounts.json`, `transactions.json`,
//! `holdings.json`, `syncs.json`). Used by every test and by `--fixtures` runs.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;

use super::wire::{self, AccountsPage, HoldingsPage, SyncsPage, TransactionsPage};
use super::{Result, SourceError, SureSource};
use crate::domain::{Account, Holding, Transaction, UpstreamHealth};

pub struct FixtureSureSource {
    dir: PathBuf,
}

impl FixtureSureSource {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    async fn load<T: DeserializeOwned>(&self, name: &str) -> Result<T> {
        let path = self.dir.join(name);
        let raw = tokio::fs::read_to_string(&path).await.map_err(|e| {
            SourceError::Io(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", path.display()),
            ))
        })?;
        serde_json::from_str(&raw)
            .map_err(|e| SourceError::Contract(format!("{}: {e}", path.display())))
    }
}

fn describe(dir: &Path) -> String {
    dir.display().to_string()
}

#[async_trait]
impl SureSource for FixtureSureSource {
    async fn accounts(&self) -> Result<Vec<Account>> {
        let page: AccountsPage = self.load("accounts.json").await?;
        Ok(page.accounts.into_iter().map(Account::from).collect())
    }

    async fn transactions(&self, since: DateTime<Utc>) -> Result<Vec<Transaction>> {
        let page: TransactionsPage = self.load("transactions.json").await?;
        let since_date = since.date_naive();
        page.transactions
            .into_iter()
            .filter(|t| t.date >= since_date)
            .map(|t| {
                Transaction::try_from(t)
                    .map_err(|e| SourceError::Contract(format!("transactions.json: {e}")))
            })
            .collect()
    }

    async fn holdings(&self) -> Result<Vec<Holding>> {
        let page: HoldingsPage = self.load("holdings.json").await?;
        Ok(wire::latest_positions(page.holdings)
            .into_iter()
            .map(Holding::from)
            .collect())
    }

    async fn health(&self) -> Result<UpstreamHealth> {
        let accounts: AccountsPage = self.load("accounts.json").await?;
        let syncs: SyncsPage = self.load("syncs.json").await.map_err(|e| {
            SourceError::Contract(format!(
                "fixture dir {} lacks readable syncs.json: {e}",
                describe(&self.dir)
            ))
        })?;
        Ok(wire::derive_health(
            &accounts.accounts,
            &syncs.data,
            Utc::now(),
        ))
    }
}
