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
    pub tenants: Vec<TenantConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub slug: String,
    pub display_name: String,
    pub upstream: UpstreamConfig,
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
            if tenant.upstream.base_url.trim().is_empty() {
                return Err(invalid(format!(
                    "tenant {:?} has an empty upstream base_url",
                    tenant.slug
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
