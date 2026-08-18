//! Serde types for Sure's REST `/api/v1` wire format, and their conversion
//! into domain types. This is the only module that knows what Sure's JSON
//! looks like. Shapes documented in docs/phase0/sure-api-surface.md and
//! pinned by tests/wire_contract.rs against the Phase 0 captures.

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;

use crate::domain::{Account, Holding, Transaction};

#[derive(Debug, Clone, Deserialize)]
pub struct Pagination {
    pub page: u32,
    pub per_page: u32,
    pub total_count: u64,
    pub total_pages: u32,
}

#[derive(Debug, Deserialize)]
pub struct AccountsPage {
    pub accounts: Vec<WireAccount>,
    pub pagination: Pagination,
}

#[derive(Debug, Deserialize)]
pub struct TransactionsPage {
    pub transactions: Vec<WireTransaction>,
    pub pagination: Pagination,
}

#[derive(Debug, Deserialize)]
pub struct HoldingsPage {
    pub holdings: Vec<WireHolding>,
    pub pagination: Pagination,
}

#[derive(Debug, Deserialize)]
pub struct SyncsPage {
    pub data: Vec<WireSync>,
    pub meta: Pagination,
}

#[derive(Debug, Deserialize)]
pub struct WireAccount {
    pub id: String,
    pub name: String,
    pub balance_cents: i64,
    pub currency: String,
    pub classification: String,
    pub account_type: String,
    pub status: String,
    pub institution_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireAccountRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct WireNamed {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct WireTransfer {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct WireTransaction {
    pub id: String,
    pub date: NaiveDate,
    pub amount_cents: i64,
    pub signed_amount_cents: i64,
    pub currency: String,
    pub name: String,
    pub classification: String,
    pub account: WireAccountRef,
    pub category: Option<WireNamed>,
    pub merchant: Option<WireNamed>,
    pub transfer: Option<WireTransfer>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct WireSecurity {
    pub ticker: Option<String>,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct WireHolding {
    pub date: NaiveDate,
    pub qty: String,
    /// Locale-formatted money string (e.g. `"$430.14"`); upstream exposes no
    /// numeric market value. See ADR-001.
    pub amount: String,
    pub currency: String,
    pub account: WireAccountRef,
    pub security: WireSecurity,
}

#[derive(Debug, Deserialize)]
pub struct WireSyncable {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireSync {
    pub status: String,
    pub syncable: Option<WireSyncable>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl From<WireAccount> for Account {
    fn from(w: WireAccount) -> Self {
        Account {
            external_id: w.id,
            name: w.name,
            institution: w.institution_name,
            kind: w.account_type,
            status: w.status,
            balance_minor: w.balance_cents,
            currency: w.currency,
        }
    }
}

impl TryFrom<WireTransaction> for Transaction {
    type Error = String;

    /// Fails on internally inconsistent money data. Upstream contract drift
    /// must be a loud error, never silently-stored wrong financial data.
    fn try_from(w: WireTransaction) -> std::result::Result<Self, Self::Error> {
        let signed_abs = w
            .signed_amount_cents
            .checked_abs()
            .ok_or_else(|| format!("transaction {}: signed_amount_cents out of range", w.id))?;
        if w.amount_cents < 0 {
            return Err(format!(
                "transaction {}: negative amount_cents {}",
                w.id, w.amount_cents
            ));
        }
        if signed_abs != w.amount_cents {
            return Err(format!(
                "transaction {}: |signed_amount_cents| {} != amount_cents {}",
                w.id, signed_abs, w.amount_cents
            ));
        }
        let sign_consistent = match w.classification.as_str() {
            "income" => w.signed_amount_cents >= 0,
            "expense" => w.signed_amount_cents <= 0,
            other => {
                return Err(format!(
                    "transaction {}: unknown classification {other:?}",
                    w.id
                ));
            }
        };
        if !sign_consistent {
            return Err(format!(
                "transaction {}: classification {:?} contradicts signed_amount_cents {}",
                w.id, w.classification, w.signed_amount_cents
            ));
        }

        let counterparty_key = w
            .merchant
            .as_ref()
            .map(|m| normalize_counterparty(&m.name))
            .filter(|k| !k.is_empty())
            .or_else(|| Some(normalize_counterparty(&w.name)))
            .filter(|k| !k.is_empty());
        Ok(Transaction {
            external_id: w.id,
            account_external_id: w.account.id,
            posted_at: w.date,
            amount_minor: w.signed_amount_cents,
            currency: w.currency,
            description: w.name,
            category: w.category.map(|c| c.name),
            counterparty_key,
            is_transfer: w.transfer.is_some(),
            updated_at: w.updated_at,
        })
    }
}

impl From<WireHolding> for Holding {
    fn from(w: WireHolding) -> Self {
        let market_value_minor = parse_formatted_money(&w.amount, &w.currency);
        if market_value_minor.is_none() {
            tracing::warn!(
                amount = %w.amount,
                currency = %w.currency,
                ticker = w.security.ticker.as_deref().unwrap_or("?"),
                "could not strictly parse holding market value; storing NULL"
            );
        }
        Holding {
            account_external_id: w.account.id,
            symbol: w.security.ticker.unwrap_or(w.security.name),
            quantity: w.qty,
            market_value_minor,
            currency: w.currency,
            as_of_date: w.date,
        }
    }
}

/// Derive per-institution health from the accounts and syncs endpoints.
/// Only institution-linked accounts contribute; a failed sync never advances
/// the success timestamp. Institutions whose accounts have no successful sync
/// at all still appear, with `None` — absence of evidence must be visible to
/// the sync-stale rule, not silently dropped.
pub fn derive_health(
    accounts: &[WireAccount],
    syncs: &[WireSync],
    fetched_at: chrono::DateTime<Utc>,
) -> crate::domain::UpstreamHealth {
    use std::collections::BTreeMap;

    let institution_by_account: BTreeMap<&str, &str> = accounts
        .iter()
        .filter_map(|a| a.institution_name.as_deref().map(|i| (a.id.as_str(), i)))
        .collect();

    let mut last_success: BTreeMap<&str, Option<chrono::DateTime<Utc>>> = institution_by_account
        .values()
        .map(|i| (*i, None))
        .collect();

    for sync in syncs {
        if sync.status != "completed" {
            continue;
        }
        let (Some(syncable), Some(completed_at)) = (&sync.syncable, sync.completed_at) else {
            continue;
        };
        // Membership in the account map is the real test; the syncable kind
        // string is upstream vocabulary that may drift.
        let Some(institution) = institution_by_account.get(syncable.id.as_str()) else {
            continue;
        };
        let entry = last_success.entry(institution).or_insert(None);
        if entry.is_none_or(|current| completed_at > current) {
            *entry = Some(completed_at);
        }
    }

    crate::domain::UpstreamHealth {
        fetched_at,
        institutions: last_success
            .into_iter()
            .map(|(institution, ts)| crate::domain::InstitutionHealth {
                institution: institution.to_string(),
                last_successful_sync_at: ts,
            })
            .collect(),
    }
}

/// Lowercase, trim, and collapse internal whitespace — a stable identity for
/// the new-counterparty rule without being clever about it.
fn normalize_counterparty(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Minor-unit exponent per ISO 4217 for the currencies Sure realistically
/// serves. Unknown currencies fall back to 2, which matches Sure's own
/// default behavior.
fn currency_exponent(currency: &str) -> u32 {
    match currency {
        "JPY" | "KRW" | "VND" => 0,
        "BHD" | "KWD" | "OMR" | "TND" | "JOD" | "IQD" | "LYD" => 3,
        _ => 2,
    }
}

fn currency_symbol(currency: &str) -> Option<&'static str> {
    match currency {
        "USD" | "CAD" | "AUD" | "NZD" | "SGD" | "HKD" | "MXN" => Some("$"),
        "EUR" => Some("€"),
        "GBP" => Some("£"),
        "JPY" | "CNY" => Some("¥"),
        "KRW" => Some("₩"),
        "INR" => Some("₹"),
        "CHF" => Some("CHF"),
        _ => None,
    }
}

/// Strictly parse a locale-formatted money string (`"$1,234.56"`) into minor
/// units for the given ISO currency. Returns `None` on anything unexpected —
/// never a guessed value.
///
/// Accepted shape: optional `-`, the currency's symbol, digits grouped in
/// threes by commas (or ungrouped), and — for currencies with a nonzero
/// exponent — a `.` followed by exactly `exponent` digits. This matches
/// Sure's `Money#format` output for symbol-prefix currencies; layouts we have
/// not verified (e.g. `1.234,56 €`) are refused so a wrong number can never
/// enter the store.
pub fn parse_formatted_money(formatted: &str, currency: &str) -> Option<i64> {
    let symbol = currency_symbol(currency)?;
    let exponent = currency_exponent(currency);

    let (negative, rest) = match formatted.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, formatted),
    };
    let rest = rest.strip_prefix(symbol)?;

    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (rest, None),
    };

    // Integer part: plain digits, or comma-grouped in strict groups of three.
    let groups: Vec<&str> = int_part.split(',').collect();
    let valid_int = if groups.len() == 1 {
        !groups[0].is_empty() && groups[0].bytes().all(|b| b.is_ascii_digit())
    } else {
        let (first, tail) = groups.split_first()?;
        !first.is_empty()
            && first.len() <= 3
            && first.bytes().all(|b| b.is_ascii_digit())
            && tail
                .iter()
                .all(|g| g.len() == 3 && g.bytes().all(|b| b.is_ascii_digit()))
    };
    if !valid_int {
        return None;
    }
    let int_digits: String = int_part.chars().filter(|c| *c != ',').collect();

    let frac_digits = match (exponent, frac_part) {
        (0, None) => String::new(),
        (0, Some(_)) => return None,
        (n, Some(f)) if f.len() == n as usize && f.bytes().all(|b| b.is_ascii_digit()) => {
            f.to_string()
        }
        _ => return None,
    };

    let magnitude: i64 = format!("{int_digits}{frac_digits}").parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// Reduce a dated holdings series to the newest row per (account, security).
/// `/api/v1/holdings` returns one row per (account, security, date) —
/// verified in Phase 0 (`total_count: 3342`, chronological order).
pub fn latest_positions(holdings: Vec<WireHolding>) -> Vec<WireHolding> {
    use std::collections::BTreeMap;

    let mut latest: BTreeMap<(String, String), WireHolding> = BTreeMap::new();
    for holding in holdings {
        let key = (
            holding.account.id.clone(),
            holding
                .security
                .ticker
                .clone()
                .unwrap_or_else(|| holding.security.name.clone()),
        );
        match latest.get(&key) {
            Some(existing) if existing.date >= holding.date => {}
            _ => {
                latest.insert(key, holding);
            }
        }
    }
    latest.into_values().collect()
}
