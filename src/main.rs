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
    /// One full cycle: sync, evaluate, deliver (the systemd timer target)
    Run {
        /// Required: run exactly one cycle and exit (veille has no scheduler)
        #[arg(long)]
        once: bool,
        /// Only this tenant (default: all configured tenants)
        #[arg(long)]
        tenant: Option<String>,
        /// Ignore the incremental watermark and pull full history
        #[arg(long)]
        full: bool,
    },
    /// Render the digest for a period to stdout
    Digest {
        /// Only this tenant (default: all configured tenants)
        #[arg(long)]
        tenant: Option<String>,
        /// Render as of this RFC 3339 instant (default: now)
        #[arg(long)]
        as_of: Option<String>,
        /// Emit the HTML variant instead of plain text
        #[arg(long)]
        html: bool,
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
        Command::Digest {
            tenant,
            as_of,
            html,
        } => digest_command(&config, tenant, as_of, html).await,
        Command::Run { once, tenant, full } => run_command(&config, once, tenant, full).await,
    }
}

const DEFAULT_DIGEST_PERIOD_DAYS: u32 = 7;

struct LlmNarrator {
    client: reqwest::Client,
    config: veille::narrate::LlmConfig,
}

#[async_trait::async_trait]
impl veille::run::Narrator for LlmNarrator {
    async fn narrate(&self, input: &veille::digest::DigestInput) -> Option<String> {
        veille::narrate::narrate(&self.client, &self.config, input).await
    }
}

async fn run_command(
    config: &Config,
    once: bool,
    only_tenant: Option<String>,
    full: bool,
) -> Result<(), String> {
    if !once {
        // No scheduler in the binary (PRP: invocation is a systemd timer).
        return Err("veille run requires --once; scheduling belongs to the timer".into());
    }
    let now = chrono::Utc::now();
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

    let narrator =
        veille::narrate::LlmConfig::resolve(config.llm.as_ref().and_then(|l| l.model.as_deref()))
            .and_then(|llm| {
                veille::narrate::LlmConfig::client().map(|client| LlmNarrator {
                    client,
                    config: llm,
                })
            });

    let mut failures = Vec::new();
    let email: Option<Box<dyn veille::deliver::EmailSender>> = match &config.smtp {
        Some(section) => match veille::deliver::smtp::LettreSmtp::new(section) {
            Ok(transport) => Some(Box::new(transport)),
            Err(e) => {
                // Broken SMTP must not stop push-only delivery.
                tracing::error!(error = %e, "smtp setup failed; digests disabled this run");
                failures.push(format!("smtp setup: {e}"));
                None
            }
        },
        None => None,
    };
    for tenant_config in selected {
        let source: Box<dyn SureSource> = match build_source(tenant_config, &None) {
            Ok(source) => source,
            Err(message) => {
                tracing::error!(tenant = %tenant_config.slug, error = %message, "source setup failed");
                failures.push(format!("{}: {message}", tenant_config.slug));
                continue;
            }
        };
        let push: Option<Box<dyn veille::deliver::PushSender>> = match &tenant_config.push {
            Some(push_config) => {
                let token = push_config
                    .token_env
                    .as_deref()
                    .map(|name| {
                        std::env::var(name)
                            .map_err(|_| format!("environment variable {name} is not set"))
                    })
                    .transpose();
                match token {
                    Ok(token) => {
                        match veille::deliver::push::NtfyPush::new(push_config.url.clone(), token) {
                            Ok(sender) => Some(Box::new(sender)),
                            Err(e) => {
                                failures.push(format!("{}: push setup: {e}", tenant_config.slug));
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        failures.push(format!("{}: {e}", tenant_config.slug));
                        continue;
                    }
                }
            }
            None => None,
        };

        let tenant = match store
            .tenants()
            .ensure(&tenant_config.slug, &tenant_config.display_name)
            .await
        {
            Ok(tenant) => tenant,
            Err(e) => {
                failures.push(format!("{}: {e}", tenant_config.slug));
                continue;
            }
        };

        let options = veille::run::RunOptions {
            sync: veille::sync::SyncOptions {
                lookback_days: tenant_config
                    .lookback_days
                    .unwrap_or(veille::sync::DEFAULT_LOOKBACK_DAYS),
                full,
            },
            narrator: narrator.as_ref().map(|n| n as &dyn veille::run::Narrator),
        };

        match veille::run::run_tenant_once(
            &store,
            tenant,
            tenant_config,
            source.as_ref(),
            push.as_deref(),
            email.as_deref(),
            now,
            options,
        )
        .await
        {
            Ok(outcome) => {
                println!(
                    "{}: {} active findings, {} alerts pushed, digest {}",
                    tenant_config.slug,
                    outcome.findings_active,
                    outcome.alerts_pushed,
                    if outcome.digest_sent {
                        "sent"
                    } else {
                        "not due"
                    },
                );
            }
            Err(e) => {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "run failed");
                failures.push(format!("{}: {e}", tenant_config.slug));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "run failed for {} tenant(s): {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

async fn digest_command(
    config: &Config,
    only_tenant: Option<String>,
    as_of: Option<String>,
    html: bool,
) -> Result<(), String> {
    let now = parse_as_of(as_of)?;
    if !config.store_path.exists() {
        return Err(format!(
            "store {} does not exist — run `veille sync` first",
            config.store_path.display()
        ));
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

    let llm =
        veille::narrate::LlmConfig::resolve(config.llm.as_ref().and_then(|l| l.model.as_deref()));
    let llm_client = veille::narrate::LlmConfig::client();

    let mut failures = Vec::new();
    for tenant_config in selected {
        let tenant = match store.tenants().by_slug(&tenant_config.slug).await {
            Ok(tenant) => tenant,
            Err(e) => {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "tenant unavailable");
                failures.push(format!("{}: {e}", tenant_config.slug));
                continue;
            }
        };
        let period_days = tenant_config
            .digest_period_days
            .unwrap_or(DEFAULT_DIGEST_PERIOD_DAYS);

        let rendered = async {
            let mut input = veille::digest::build_digest_input(
                &store,
                tenant,
                &tenant_config.display_name,
                period_days,
                now,
            )
            .await
            .map_err(|e| e.to_string())?;
            if let (Some(llm), Some(client)) = (&llm, &llm_client) {
                input.narration = veille::narrate::narrate(client, llm, &input).await;
            }
            if html {
                veille::digest::render_html(&input).map_err(|e| e.to_string())
            } else {
                veille::digest::render_text(&input).map_err(|e| e.to_string())
            }
        }
        .await;

        match rendered {
            Ok(text) => println!("{text}"),
            Err(e) => {
                tracing::error!(tenant = %tenant_config.slug, error = %e, "digest failed");
                failures.push(format!("{}: {e}", tenant_config.slug));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "digest failed for {} tenant(s): {}",
            failures.len(),
            failures.join("; ")
        ))
    }
}

fn parse_as_of(as_of: Option<String>) -> Result<chrono::DateTime<chrono::Utc>, String> {
    match as_of {
        Some(raw) => chrono::DateTime::parse_from_rfc3339(&raw)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .map_err(|e| format!("--as-of must be RFC 3339 (e.g. 2026-08-20T22:00:00Z): {e}")),
        None => Ok(chrono::Utc::now()),
    }
}

async fn evaluate_command(
    config: &Config,
    only_tenant: Option<String>,
    fixtures: Option<PathBuf>,
    as_of: Option<String>,
    dry_run: bool,
) -> Result<(), String> {
    let now = parse_as_of(as_of)?;
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
