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
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
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
    }
}

async fn sync_command(
    config: &Config,
    only_tenant: Option<String>,
    fixtures: Option<PathBuf>,
    full: bool,
) -> Result<(), String> {
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
        let source: Box<dyn SureSource> = match &fixtures {
            Some(dir) => Box::new(FixtureSureSource::new(dir.clone())),
            None => {
                let key = std::env::var(&tenant_config.upstream.api_key_env).map_err(|_| {
                    format!(
                        "environment variable {} (api key for tenant {:?}) is not set",
                        tenant_config.upstream.api_key_env, tenant_config.slug
                    )
                })?;
                Box::new(
                    ApiSureSource::new(&tenant_config.upstream.base_url, key)
                        .map_err(|e| e.to_string())?,
                )
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
