//! Real [`SureSource`] over Sure's REST `/api/v1` (ADR-001), authenticated
//! with a per-tenant `read`-scoped API key.
//!
//! The HTTP layer is structurally read-only: [`ReadOnlyHttp`] exposes exactly
//! one operation, a GET. There is no way to send a mutating verb through this
//! module, matching invariant §2.1 on our side of the wire (the server
//! enforces it on its side by rejecting writes from `read`-scoped keys).

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::de::DeserializeOwned;

use super::wire::{self, AccountsPage, HoldingsPage, SyncsPage, TransactionsPage, WireSync};
use super::{Result, SourceError, SureSource};
use crate::domain::{Account, Holding, Transaction, UpstreamHealth};

/// Upstream caps `per_page` at 100.
const PER_PAGE: u32 = 100;
/// Health scans at most this many pages of syncs. An institution with no
/// successful sync among the newest 1000 sync records reports `None`, which
/// the sync-stale rule reads as "stale" — the conservative direction.
const MAX_SYNC_PAGES: u32 = 10;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A GET-only HTTP client. Deliberately incapable of any other verb.
struct ReadOnlyHttp {
    client: reqwest::Client,
    base: Url,
    api_key: String,
}

impl ReadOnlyHttp {
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        let url = self
            .base
            .join(path)
            .map_err(|e| SourceError::Request(format!("invalid path {path}: {e}")))?;
        let response = self
            .client
            .get(url.clone())
            .header("X-Api-Key", &self.api_key)
            .query(query)
            .send()
            .await
            .map_err(|e| SourceError::Request(format!("GET {path}: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            // Do not echo the body: error payloads are upstream-controlled
            // and belong in upstream logs, not ours.
            return Err(SourceError::Request(format!(
                "GET {path} returned HTTP {status}"
            )));
        }
        response
            .json::<T>()
            .await
            .map_err(|e| SourceError::Contract(format!("GET {path}: {e}")))
    }
}

pub struct ApiSureSource {
    http: ReadOnlyHttp,
}

impl ApiSureSource {
    pub fn new(base_url: &str, api_key: String) -> Result<Self> {
        let base = Url::parse(base_url)
            .map_err(|e| SourceError::Request(format!("invalid base url: {e}")))?;
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| SourceError::Request(format!("http client init: {e}")))?;
        Ok(Self {
            http: ReadOnlyHttp {
                client,
                base,
                api_key,
            },
        })
    }

    async fn fetch_all_accounts(&self) -> Result<Vec<super::wire::WireAccount>> {
        let mut page = 1u32;
        let mut out = Vec::new();
        loop {
            let body: AccountsPage = self
                .http
                .get_json("/api/v1/accounts", &page_query(page, &[]))
                .await?;
            let total_pages = body.pagination.total_pages;
            out.extend(body.accounts);
            if page >= total_pages {
                return Ok(out);
            }
            page += 1;
        }
    }
}

fn page_query(page: u32, extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    let mut q = vec![
        ("page", page.to_string()),
        ("per_page", PER_PAGE.to_string()),
    ];
    q.extend(extra.iter().cloned());
    q
}

#[async_trait]
impl SureSource for ApiSureSource {
    async fn accounts(&self) -> Result<Vec<Account>> {
        Ok(self
            .fetch_all_accounts()
            .await?
            .into_iter()
            .map(Account::from)
            .collect())
    }

    async fn transactions(&self, since: DateTime<Utc>) -> Result<Vec<Transaction>> {
        let extra: Vec<(&'static str, String)> = if since > DateTime::<Utc>::MIN_UTC {
            vec![("start_date", since.date_naive().to_string())]
        } else {
            Vec::new()
        };

        let mut page = 1u32;
        let mut out = Vec::new();
        loop {
            let body: TransactionsPage = self
                .http
                .get_json("/api/v1/transactions", &page_query(page, &extra))
                .await?;
            let total_pages = body.pagination.total_pages;
            out.extend(body.transactions.into_iter().map(Transaction::from));
            if page >= total_pages {
                return Ok(out);
            }
            page += 1;
        }
    }

    async fn holdings(&self) -> Result<Vec<Holding>> {
        let mut page = 1u32;
        let mut out = Vec::new();
        loop {
            let body: HoldingsPage = self
                .http
                .get_json("/api/v1/holdings", &page_query(page, &[]))
                .await?;
            let total_pages = body.pagination.total_pages;
            out.extend(body.holdings.into_iter().map(Holding::from));
            if page >= total_pages {
                return Ok(out);
            }
            page += 1;
        }
    }

    async fn health(&self) -> Result<UpstreamHealth> {
        let accounts = self.fetch_all_accounts().await?;

        let mut syncs: Vec<WireSync> = Vec::new();
        let mut page = 1u32;
        loop {
            let body: SyncsPage = self
                .http
                .get_json("/api/v1/syncs", &page_query(page, &[]))
                .await?;
            let total_pages = body.meta.total_pages;
            syncs.extend(body.data);
            if page >= total_pages || page >= MAX_SYNC_PAGES {
                break;
            }
            page += 1;
        }

        Ok(wire::derive_health(&accounts, &syncs, Utc::now()))
    }
}
