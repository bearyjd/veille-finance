//! Deployment configuration: tenants, upstreams, store path. Secrets are
//! never in the file — the config names an environment variable per upstream
//! and the key material is resolved from the environment at use time.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {message}")]
    Invalid { path: String, message: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub store_path: PathBuf,
    /// Optional narration settings. The endpoint and key come from the
    /// environment (PRP §8); only the model name lives here.
    #[serde(default)]
    pub llm: Option<LlmSection>,
    pub tenants: Vec<TenantConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub slug: String,
    pub display_name: String,
    /// Incremental sync window in days (default: [`crate::sync::DEFAULT_LOOKBACK_DAYS`]).
    /// Upstream backfills older than this are only caught by a `--full` resync.
    pub lookback_days: Option<u32>,
    pub upstream: UpstreamConfig,
    /// Per-tenant rule thresholds (PRP §7: thresholds live in config, not code).
    #[serde(default)]
    pub rules: RuleThresholds,
    /// Days each digest covers (default 7).
    pub digest_period_days: Option<u32>,
}

/// Defaults are conservative: a tool that cries wolf gets muted, and a muted
/// tool is a tool that missed the real thing (PRP §7).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RuleThresholds {
    /// `sync-stale`: alert when an institution has no successful sync in this
    /// many days. SimpleFIN refreshes ~daily with quota, so 4, not 1.
    pub sync_stale_days: u32,
    /// `dormant-card-wake`: an account is dormant after this many days
    /// without activity.
    pub dormant_days: u32,
    /// `balance-band`: rolling window the balance baseline is computed over.
    pub balance_band_window_days: u32,
    /// `large-transfer`: absolute per-transaction threshold in minor units.
    /// `None` disables the rule — set it per tenant once real baselines
    /// exist rather than guessing.
    pub large_transfer_minor: Option<i64>,
    /// `new-counterparty`: only first-ever payees at or above this amount
    /// (minor units) fire.
    pub new_counterparty_floor_minor: i64,
    /// `recurring-missing`: months a series must appear in before it counts
    /// as recurring.
    pub recurring_min_occurrences: u32,
    /// `recurring-missing`: +/- days around the expected day of month.
    pub recurring_day_window: u32,
}

impl Default for RuleThresholds {
    fn default() -> Self {
        Self {
            sync_stale_days: 4,
            dormant_days: 60,
            balance_band_window_days: 60,
            large_transfer_minor: None,
            new_counterparty_floor_minor: 50_000,
            recurring_min_occurrences: 3,
            recurring_day_window: 5,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub base_url: String,
    /// Name of the environment variable holding this tenant's read-scoped
    /// API key. The key itself must never appear in the file.
    pub api_key_env: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let display = path.display().to_string();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: display.clone(),
            source,
        })?;
        let config: Config = toml::from_str(&raw).map_err(|e| ConfigError::Invalid {
            path: display.clone(),
            message: e.to_string(),
        })?;
        config.validate(&display)?;
        Ok(config)
    }

    fn validate(&self, path: &str) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            path: path.to_string(),
            message,
        };

        if self.tenants.is_empty() {
            return Err(invalid("at least one [[tenants]] entry is required".into()));
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut seen_base_urls = std::collections::BTreeSet::new();
        let mut seen_key_envs = std::collections::BTreeSet::new();
        for tenant in &self.tenants {
            if !is_valid_slug(&tenant.slug) {
                return Err(invalid(format!(
                    "tenant slug {:?} is invalid: use lowercase letters, digits, and dashes",
                    tenant.slug
                )));
            }
            if !seen.insert(tenant.slug.as_str()) {
                return Err(invalid(format!("duplicate tenant slug {:?}", tenant.slug)));
            }
            if tenant.display_name.trim().is_empty() {
                return Err(invalid(format!(
                    "tenant {:?} has an empty display_name",
                    tenant.slug
                )));
            }
            if !is_plausible_env_name(&tenant.upstream.api_key_env) {
                return Err(invalid(format!(
                    "tenant {:?}: api_key_env {:?} does not look like an environment \
                     variable name — it must reference the variable holding the key, \
                     never the key itself",
                    tenant.slug, tenant.upstream.api_key_env
                )));
            }
            if let Some(days) = tenant.digest_period_days
                && !(1..=366).contains(&days)
            {
                return Err(invalid(format!(
                    "tenant {:?}: digest_period_days {days} must be between 1 and 366",
                    tenant.slug
                )));
            }
            let base_url = tenant.upstream.base_url.trim();
            if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                return Err(invalid(format!(
                    "tenant {:?}: base_url {:?} must use http:// or https://",
                    tenant.slug, tenant.upstream.base_url
                )));
            }
            // One upstream = one tenant. Two tenants sharing a base_url or a
            // key would silently write one household's data into another
            // household's rows — the only cross-tenant path this design has.
            if !seen_base_urls.insert(base_url) {
                return Err(invalid(format!(
                    "tenant {:?}: base_url {base_url:?} is already used by another tenant",
                    tenant.slug
                )));
            }
            if !seen_key_envs.insert(tenant.upstream.api_key_env.as_str()) {
                return Err(invalid(format!(
                    "tenant {:?}: api_key_env {:?} is already used by another tenant",
                    tenant.slug, tenant.upstream.api_key_env
                )));
            }
        }
        Ok(())
    }
}

fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Conventional environment variable names: uppercase, digits, underscores,
/// not starting with a digit, reasonably short. Anything else — in particular
/// a pasted token — is rejected.
fn is_plausible_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}
