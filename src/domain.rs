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

/// Severity is set by the rule that emitted the finding, and by nothing else
/// (PRP §2.4): `Info` appears in the digest, `Warn` is highlighted there,
/// `Alert` additionally triggers immediate push delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Alert,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Alert => "alert",
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "info" => Ok(Severity::Info),
            "warn" => Ok(Severity::Warn),
            "alert" => Ok(Severity::Alert),
            other => Err(format!("unknown severity {other:?}")),
        }
    }
}

/// A condition detected by deterministic rule code. Only rules create these;
/// nothing downstream may add, drop, or re-rank them.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub rule_id: String,
    pub severity: Severity,
    /// Human-readable identity of what the finding is about
    /// (e.g. `institution:First National`, `account:Chase Sapphire`).
    pub subject: String,
    /// One plain sentence a person reads in the digest. Written by the rule —
    /// the deterministic rendering is the product, not the LLM prose.
    pub summary: String,
    /// Structured details; all money as integer minor units plus currency.
    pub evidence: serde_json::Value,
    /// Stable identity of the condition instance — the store is unique on
    /// `(tenant_id, dedupe_key)`, so re-detecting the same condition never
    /// re-alerts.
    pub dedupe_key: String,
    pub detected_at: DateTime<Utc>,
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

/// Currency minor-unit exponent (ISO 4217) for the currencies Sure
/// realistically serves; unknown currencies use 2, matching Sure.
pub fn currency_exponent(currency: &str) -> u32 {
    match currency {
        "JPY" | "KRW" | "VND" => 0,
        "BHD" | "KWD" | "OMR" | "TND" | "JOD" | "IQD" | "LYD" => 3,
        _ => 2,
    }
}

pub fn currency_symbol(currency: &str) -> Option<&'static str> {
    match currency {
        "USD" | "CAD" | "AUD" | "NZD" | "SGD" | "HKD" | "MXN" => Some("$"),
        "EUR" => Some("€"),
        "GBP" => Some("£"),
        "JPY" | "CNY" => Some("¥"),
        "KRW" => Some("₩"),
        "INR" => Some("₹"),
        _ => None,
    }
}

/// Render integer minor units for humans: symbol (or ISO code prefix),
/// comma thousands grouping, exponent-correct decimals. Display only —
/// never parsed back.
pub fn format_minor(minor: i64, currency: &str) -> String {
    let exponent = currency_exponent(currency);
    let divisor = 10u64.pow(exponent);
    let magnitude = minor.unsigned_abs();
    let int_part = magnitude / divisor;
    let frac_part = magnitude % divisor;

    let digits = int_part.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }

    let number = if exponent == 0 {
        grouped
    } else {
        format!("{grouped}.{frac_part:0width$}", width = exponent as usize)
    };
    let sign = if minor < 0 { "-" } else { "" };
    match currency_symbol(currency) {
        Some(symbol) => format!("{sign}{symbol}{number}"),
        None => format!("{sign}{currency} {number}"),
    }
}
