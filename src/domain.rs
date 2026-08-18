//! Core domain types, independent of any upstream wire format.
//!
//! Money is always integer minor units (`_minor`). Sign convention:
//! positive = inflow (money into the account), negative = outflow.

use chrono::{DateTime, NaiveDate, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub external_id: String,
    pub name: String,
    /// Institution display name; `None` for manual (unlinked) accounts.
    pub institution: Option<String>,
    /// Upstream account type, e.g. "depository", "credit_card", "investment".
    pub kind: String,
    pub status: String,
    pub balance_minor: i64,
    pub currency: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// Sure's stable transaction UUID (the aggregator's own id can be null upstream).
    pub external_id: String,
    pub account_external_id: String,
    pub posted_at: NaiveDate,
    /// Signed minor units; positive = inflow.
    pub amount_minor: i64,
    pub currency: String,
    pub description: String,
    pub category: Option<String>,
    /// Normalized key identifying the counterparty, for the new-counterparty rule.
    pub counterparty_key: Option<String>,
    pub is_transfer: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holding {
    pub account_external_id: String,
    pub symbol: String,
    /// Exact decimal string as reported upstream (not money; may be fractional).
    pub quantity: String,
    /// `None` when the upstream formatted money string could not be strictly parsed.
    pub market_value_minor: Option<i64>,
    pub currency: String,
    /// The upstream valuation date of this position — holdings are a dated
    /// series upstream, and this may lag the sync time.
    pub as_of_date: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstitutionHealth {
    pub institution: String,
    pub last_successful_sync_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamHealth {
    pub fetched_at: DateTime<Utc>,
    /// One entry per linked institution. Manual accounts do not appear.
    pub institutions: Vec<InstitutionHealth>,
}
