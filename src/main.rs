use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use veille::config::Config;
use veille::source::SureSource;
use veille::source::api::ApiSureSource;
use veille::source::fixture::FixtureSureSource;
use veille::store::Store;
use veille::sync::sync_tenant;

#[derive(Parser)]
#[command(
    name = "veille",
    version,
    about = "Read-only financial watch service for Sure"
)]
struct Cli {
    /// Path to the config file
    #[arg(long, global = true, default_value = "config/veille.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Pull upstream data and materialize it into the snapshot store
    Sync {
        /// Only sync this tenant (default: all configured tenants)
        #[arg(long)]
        tenant: Option<String>,
        /// Read from a fixture directory instead of the live upstream
        #[arg(long)]
        fixtures: Option<PathBuf>,
        /// Ignore the incremental watermark and pull full history
        #[arg(long)]
        full: bool,
    },
    /// Run the rules over the store and print findings
    Evaluate {
        /// Only evaluate this tenant (default: all configured tenants)
        #[arg(long)]
        tenant: Option<String>,
        /// Sync a fixture directory into an ephemeral in-memory store and
        /// evaluate that, leaving the real store untouched
        #[arg(long)]
        fixtures: Option<PathBuf>,
        /// Evaluate as of this RFC 3339 instant (default: now); fixed values
        /// make runs reproducible and diffable
        #[arg(long)]
        as_of: Option<String>,
        /// Print findings without recording anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logs to stderr: stdout carries only findings/digest output, so
    // `evaluate --dry-run` stays diffable.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let config = Config::load(&cli.config).map_err(|e| e.to_string())?;

    match cli.command {
        Command::Sync {
            tenant,
            fixtures,
            full,
        } => sync_command(&config, tenant, fixtures, full).await,
        Command::Evaluate {
            tenant,
            fixtures,
            as_of,
            dry_run,
        } => evaluate_command(&config, tenant, fixtures, as_of, dry_run).await,
    }
}

async fn evaluate_command(
    config: &Config,
    only_tenant: Option<String>,
    fixtures: Option<PathBuf>,
    as_of: Option<String>,
    dry_run: bool,
) -> Result<(), String> {
    let now = match as_of {
        Some(raw) => chrono::DateTime::parse_from_rfc3339(&raw)
            .map_err(|e| format!("--as-of must be RFC 3339 (e.g. 2026-08-20T22:00:00Z): {e}"))?
            .with_timezone(&chrono::Utc),
        None => chrono::Utc::now(),
    };
    if fixtures.is_some() && only_tenant.is_none() && config.tenants.len() > 1 {
        return Err("--fixtures requires --tenant when more than one tenant is configured".into());
    }

    let store = match &fixtures {
        Some(_) => veille::store::Store::open_in_memory()
            .await
            .map_err(|e| e.to_string())?,
        None => {
            // A dry run must not create a store that was never synced.
            if dry_run && !config.store_path.exists() {
                return Err(format!(
                    "store {} does not exist — run `veille sync` first",
                    config.store_path.display()
                ));
            }
            Store::open(&config.store_path)
                .await
                .map_err(|e| e.to_string())?
        }
    };

    let selected: Vec<_> = config
        .tenants
        .iter()
        .filter(|t| only_tenant.as_deref().is_none_or(|slug| slug == t.slug))
        .collect();
    if selected.is_empty() {
        return Err(match only_tenant {
            Some(slug) => format!("tenant {slug:?} is not in the config"),
            None => "config contains no tenants".into(),
        });
    }

    let mut failures = Vec::new();
    for tenant_config in selected {
        // A dry run against the real store only reads: the tenant must
        // already exist from a prior sync. Any per-tenant setup failure is
        // collected so the remaining tenants still get evaluated.
        let resolved = if dry_run && fixtures.is_none() {
            store.tenants().by_slug(&tenant_config.slug).await
        } else {
            store
                .tenants()
                .ensure(&tenant_config.slug, &tenant_config.display_name)
                .await
        };
        let tenant = match resolved {
            Ok(tenant) => tenant,
            Err(e) => {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "tenant unavailable");
                failures.push(format!("{}: {e}", tenant_config.slug));
                continue;
            }
        };

        if let Some(dir) = &fixtures {
            let source = FixtureSureSource::new(dir.clone());
            if let Err(e) = sync_tenant(
                &store,
                tenant,
                &source,
                now,
                veille::sync::SyncOptions::default(),
            )
            .await
            {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "fixture sync failed");
                failures.push(format!("{}: fixture sync: {e}", tenant_config.slug));
                continue;
            }
        }

        let persist = !dry_run;
        match veille::rules::evaluate_tenant(
            &store,
            tenant,
            tenant_config.rules.clone(),
            now,
            persist,
        )
        .await
        {
            Ok(findings) => {
                for f in &findings {
                    println!(
                        "{}	{}	{}	{}	{}	{}",
                        tenant_config.slug,
                        f.severity.as_str(),
                        f.rule_id,
                        f.subject,
                        f.dedupe_key,
                        f.evidence
                    );
                }
                if findings.is_empty() {
                    tracing::info!(tenant = %tenant_config.slug, "no findings");
                }
            }
            Err(e) => {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "evaluation failed");
                failures.push(format!("{}: {e}", tenant_config.slug));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "evaluation failed for {} tenant(s): {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

fn build_source(
    tenant_config: &veille::config::TenantConfig,
    fixtures: &Option<PathBuf>,
) -> Result<Box<dyn SureSource>, String> {
    match fixtures {
        Some(dir) => Ok(Box::new(FixtureSureSource::new(dir.clone()))),
        None => {
            let key = std::env::var(&tenant_config.upstream.api_key_env).map_err(|_| {
                format!(
                    "environment variable {} (api key) is not set",
                    tenant_config.upstream.api_key_env
                )
            })?;
            Ok(Box::new(
                ApiSureSource::new(&tenant_config.upstream.base_url, key)
                    .map_err(|e| e.to_string())?,
            ))
        }
    }
}

async fn sync_command(
    config: &Config,
    only_tenant: Option<String>,
    fixtures: Option<PathBuf>,
    full: bool,
) -> Result<(), String> {
    // A fixture directory is one instance's data; syncing it into every
    // configured tenant would cross-pollute the store.
    if fixtures.is_some() && only_tenant.is_none() && config.tenants.len() > 1 {
        return Err("--fixtures requires --tenant when more than one tenant is configured".into());
    }
    let store = Store::open(&config.store_path)
        .await
        .map_err(|e| e.to_string())?;

    let selected: Vec<_> = config
        .tenants
        .iter()
        .filter(|t| only_tenant.as_deref().is_none_or(|slug| slug == t.slug))
        .collect();
    if selected.is_empty() {
        return Err(match only_tenant {
            Some(slug) => format!("tenant {slug:?} is not in the config"),
            None => "config contains no tenants".into(),
        });
    }

    let mut failures = Vec::new();
    for tenant_config in selected {
        // A broken credential or base_url for one tenant must not prevent the
        // other tenants from syncing — collect and keep going.
        let source: Box<dyn SureSource> = match build_source(tenant_config, &fixtures) {
            Ok(source) => source,
            Err(message) => {
                tracing::error!(tenant = %tenant_config.slug, error = %message, "source setup failed");
                failures.push(format!("{}: {message}", tenant_config.slug));
                continue;
            }
        };

        let tenant = store
            .tenants()
            .ensure(&tenant_config.slug, &tenant_config.display_name)
            .await
            .map_err(|e| e.to_string())?;

        let options = veille::sync::SyncOptions {
            lookback_days: tenant_config
                .lookback_days
                .unwrap_or(veille::sync::DEFAULT_LOOKBACK_DAYS),
            full,
        };
        match sync_tenant(&store, tenant, source.as_ref(), chrono::Utc::now(), options).await {
            Ok(outcome) => {
                println!(
                    "{}: {} account snapshots, {} new / {} refreshed transactions, {} holding snapshots",
                    tenant_config.slug,
                    outcome.account_snapshots,
                    outcome.transactions.inserted,
                    outcome.transactions.refreshed,
                    outcome.holding_snapshots,
                );
            }
            Err(e) => {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "sync failed");
                failures.push(format!("{}: {e}", tenant_config.slug));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "sync failed for {} tenant(s): {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}
