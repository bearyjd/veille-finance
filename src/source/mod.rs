//! The single coupling surface to Sure (PRP §5).
//!
//! This trait is read-only by construction: there is no write method, and
//! there must never be one. Implementations: [`api::ApiSureSource`] (real,
//! REST `/api/v1` per ADR-001) and [`fixture::FixtureSureSource`] (golden
//! files, used by every test).

pub mod api;
pub mod fixture;
pub mod wire;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::domain::{Account, Holding, Transaction, UpstreamHealth};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("upstream request failed: {0}")]
    Request(String),
    #[error("upstream returned unexpected data: {0}")]
    Contract(String),
    #[error("io error reading fixtures: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SourceError>;

#[async_trait]
pub trait SureSource: Send + Sync {
    async fn accounts(&self) -> Result<Vec<Account>>;
    async fn transactions(&self, since: DateTime<Utc>) -> Result<Vec<Transaction>>;
    async fn holdings(&self) -> Result<Vec<Holding>>;
    async fn health(&self) -> Result<UpstreamHealth>;
}
