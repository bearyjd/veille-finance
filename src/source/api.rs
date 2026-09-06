//! Real [`SureSource`] over Sure's REST `/api/v1` (ADR-001), authenticated
//! with a per-tenant `read`-scoped API key.
//!
//! The HTTP layer is structurally read-only: [`ReadOnlyHttp`] exposes exactly
//! one operation, a GET, and never follows redirects. There is no way to send
//! a mutating verb through this module, matching invariant §2.1 on our side
//! of the wire (the server enforces it on its side by rejecting writes from
//! `read`-scoped keys).

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::de::DeserializeOwned;

use super::wire::{
    self, AccountsPage, HoldingsPage, SyncsPage, TransactionsPage, WireAccount, WireSync,
};
use super::{Result, SourceError, SureSource};
use crate::domain::{Account, Holding, Transaction, UpstreamHealth};

/// Upstream caps `per_page` at 100.
const PER_PAGE: u32 = 100;
/// Health scans at most this many pages of syncs. An institution with no
/// successful sync among the newest 1000 sync records reports `None`, which
/// the sync-stale rule reads as "stale" — the conservative direction.
const MAX_SYNC_PAGES: u32 = 10;
/// Hard cap on data-endpoint pagination (200 pages × 100 rows = 20k rows,
/// far beyond household scale). A server whose `total_pages` keeps growing
/// gets a loud contract error instead of an unbounded crawl.
const MAX_DATA_PAGES: u32 = 200;
/// Holdings are a dated series upstream; fetch only this recent window and
/// reduce to the newest row per position. Any live account produces rows far
/// more often than this.
const HOLDINGS_WINDOW_DAYS: i64 = 90;
/// On HTTP 429, honor Retry-After up to this many attempts per request.
/// Sure's standard tier allows 100 requests/hour; the reset can be most of
/// an hour away, so waits are long but bounded.
const RATE_LIMIT_RETRIES: u32 = 2;
const RETRY_AFTER_DEFAULT: Duration = Duration::from_secs(300);
const RETRY_AFTER_CAP: Duration = Duration::from_secs(3900);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Ceiling on a single response body. `reqwest`'s `.json()` buffers the whole
/// body with no cap of its own, so a compromised or MITM'd upstream could
/// answer a paged request with gigabytes and OOM the process — the caps above
/// bound the *number* of pages and the *requested* `per_page`, neither of
/// which bounds what the server actually sends back. One page is at most
/// `PER_PAGE` small records; even with generously long names that is a few
/// hundred KB, so this leaves roughly 20x headroom over any legitimate page.
/// Mirrors `narrate/llm.rs`'s `MAX_RESPONSE_BYTES`, which already does this
/// for the other network surface.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// A GET-only HTTP client. Deliberately incapable of any other verb.
struct ReadOnlyHttp {
    client: reqwest::Client,
    base: Url,
    /// Marked sensitive so no Debug rendering ever prints the key.
    api_key: reqwest::header::HeaderValue,
}

impl ReadOnlyHttp {
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T> {
        // Relative join so a base_url with a path prefix keeps it.
        let url = self
            .base
            .join(path.trim_start_matches('/'))
            .map_err(|e| SourceError::Request(format!("invalid path {path}: {e}")))?;

        let mut rate_limit_attempts = 0u32;
        loop {
            let mut response = self
                .client
                .get(url.clone())
                .header("X-Api-Key", self.api_key.clone())
                .query(query)
                .send()
                .await
                .map_err(|e| SourceError::Request(format!("GET {path}: {e}")))?;

            let status = response.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                && rate_limit_attempts < RATE_LIMIT_RETRIES
            {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(RETRY_AFTER_DEFAULT)
                    .min(RETRY_AFTER_CAP);
                rate_limit_attempts += 1;
                tracing::warn!(
                    path,
                    wait_secs = wait.as_secs(),
                    attempt = rate_limit_attempts,
                    "rate limited by upstream; honoring Retry-After"
                );
                tokio::time::sleep(wait).await;
                continue;
            }
            if !status.is_success() {
                // Do not echo the body: error payloads are upstream-controlled
                // and belong in upstream logs, not ours.
                return Err(SourceError::Request(format!(
                    "GET {path} returned HTTP {status}"
                )));
            }
            // Bounded read rather than `.json()`: see MAX_RESPONSE_BYTES.
            // The body is upstream-controlled, so its size is too.
            let mut body_bytes: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| SourceError::Request(format!("GET {path}: read: {e}")))?
            {
                body_bytes.extend_from_slice(&chunk);
                if body_bytes.len() > MAX_RESPONSE_BYTES {
                    return Err(SourceError::Contract(format!(
                        "GET {path}: response exceeded {MAX_RESPONSE_BYTES} bytes"
                    )));
                }
            }
            return serde_json::from_slice(&body_bytes)
                .map_err(|e| SourceError::Contract(format!("GET {path}: {e}")));
        }
    }
}

pub struct ApiSureSource {
    http: ReadOnlyHttp,
    /// One accounts fetch per source instance (one instance = one sync run):
    /// `accounts()` and `health()` share it, saving a full page sweep against
    /// the 100 req/h budget.
    accounts_cache: tokio::sync::OnceCell<Vec<WireAccount>>,
}

impl ApiSureSource {
    pub fn new(base_url: &str, api_key: String) -> Result<Self> {
        // Normalize to a trailing slash so relative joins preserve any path
        // prefix in the configured base_url.
        let normalized = if base_url.ends_with('/') {
            base_url.to_string()
        } else {
            format!("{base_url}/")
        };
        let base = Url::parse(&normalized)
            .map_err(|e| SourceError::Request(format!("invalid base url: {e}")))?;
        let mut api_key = reqwest::header::HeaderValue::from_str(&api_key)
            .map_err(|_| SourceError::Request("api key contains invalid header bytes".into()))?;
        api_key.set_sensitive(true);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // Never follow redirects: Sure's API does not legitimately
            // redirect, and following one would forward X-Api-Key (which
            // reqwest's cross-host sanitization does NOT strip, unlike
            // Authorization) to an arbitrary host.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SourceError::Request(format!("http client init: {e}")))?;
        Ok(Self {
            http: ReadOnlyHttp {
                client,
                base,
                api_key,
            },
            accounts_cache: tokio::sync::OnceCell::new(),
        })
    }

    /// Fetch every page of a paginated endpoint, bounded by
    /// [`MAX_DATA_PAGES`], deduplicating rows by key (a server that ignores
    /// the `page` parameter must not double-count anything).
    async fn fetch_paged<P, T, K>(
        &self,
        path: &'static str,
        extra: &[(&'static str, String)],
        split: impl Fn(P) -> (Vec<T>, u32),
        dedupe_key: impl Fn(&T) -> K,
    ) -> Result<Vec<T>>
    where
        P: DeserializeOwned,
        K: Ord,
    {
        let mut page = 1u32;
        let mut out: Vec<T> = Vec::new();
        let mut seen: BTreeSet<K> = BTreeSet::new();
        loop {
            let body: P = self.http.get_json(path, &page_query(page, extra)).await?;
            let (items, total_pages) = split(body);
            for item in items {
                if seen.insert(dedupe_key(&item)) {
                    out.push(item);
                } else {
                    tracing::warn!(path, page, "duplicate row across pages ignored");
                }
            }
            if page >= total_pages {
                return Ok(out);
            }
            if page >= MAX_DATA_PAGES {
                return Err(SourceError::Contract(format!(
                    "GET {path}: pagination exceeded {MAX_DATA_PAGES} pages \
                     (server reports total_pages={total_pages})"
                )));
            }
            page += 1;
        }
    }

    async fn cached_accounts(&self) -> Result<&Vec<WireAccount>> {
        self.accounts_cache
            .get_or_try_init(|| async {
                self.fetch_paged(
                    "/api/v1/accounts",
                    &[],
                    |p: AccountsPage| (p.accounts, p.pagination.total_pages),
                    |a: &WireAccount| a.id.clone(),
                )
                .await
            })
            .await
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
            .cached_accounts()
            .await?
            .iter()
            .map(|a| {
                Account::from(WireAccount {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    balance_cents: a.balance_cents,
                    currency: a.currency.clone(),
                    classification: a.classification.clone(),
                    account_type: a.account_type.clone(),
                    status: a.status.clone(),
                    institution_name: a.institution_name.clone(),
                })
            })
            .collect())
    }

    async fn transactions(&self, since: DateTime<Utc>) -> Result<Vec<Transaction>> {
        let extra: Vec<(&'static str, String)> = if since > DateTime::<Utc>::MIN_UTC {
            vec![("start_date", since.date_naive().to_string())]
        } else {
            Vec::new()
        };

        let wire_transactions = self
            .fetch_paged(
                "/api/v1/transactions",
                &extra,
                |p: TransactionsPage| (p.transactions, p.pagination.total_pages),
                |t: &wire::WireTransaction| t.id.clone(),
            )
            .await?;

        wire_transactions
            .into_iter()
            .map(|t| {
                Transaction::try_from(t)
                    .map_err(|e| SourceError::Contract(format!("GET /api/v1/transactions: {e}")))
            })
            .collect()
    }

    async fn holdings(&self) -> Result<Vec<Holding>> {
        // The endpoint is a dated series (one row per account, security,
        // date). Fetch a bounded recent window, dedupe exact rows, and keep
        // only the newest valuation per position.
        let window_start =
            (Utc::now().date_naive() - chrono::Duration::days(HOLDINGS_WINDOW_DAYS)).to_string();
        let extra: Vec<(&'static str, String)> = vec![("start_date", window_start)];
        let wire_holdings = self
            .fetch_paged(
                "/api/v1/holdings",
                &extra,
                |p: HoldingsPage| (p.holdings, p.pagination.total_pages),
                |h: &wire::WireHolding| {
                    (
                        h.account.id.clone(),
                        h.security
                            .ticker
                            .clone()
                            .unwrap_or_else(|| h.security.name.clone()),
                        h.date,
                    )
                },
            )
            .await?;
        Ok(wire::latest_positions(wire_holdings)
            .into_iter()
            .map(Holding::from)
            .collect())
    }

    async fn health(&self) -> Result<UpstreamHealth> {
        let accounts = self.cached_accounts().await?;

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

        Ok(wire::derive_health(accounts, &syncs, Utc::now()))
    }
}
