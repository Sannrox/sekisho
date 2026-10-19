//! `sekisho serve` runs the gateway; `sekisho registry` edits the tool
//! registry; `sekisho diagnostics` prints a support bundle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use sekisho::config::Config;
use sekisho::gateway::Federation;
use sekisho::registry::{self, Registry};
use sekisho::server::{AppState, router};
use sekisho::verifier::Verifier;

#[derive(Parser)]
#[command(
    name = "sekisho",
    version,
    about = "Governed tool gateway for AI agents"
)]
struct Cli {
    #[arg(long, global = true, default_value = "sekisho.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve `/mcp`, its OAuth metadata, `/healthz` and `/metrics`. Prints
    /// `listening on <addr>` once bound.
    Serve,
    /// Print a JSON support bundle with no secrets.
    Diagnostics,
    /// Review and change registry tools. Edits are audited.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
}

#[derive(Subcommand)]
enum RegistryCommand {
    /// List tools with their state.
    List,
    /// Show one tool's approved and pending versions.
    Show { tool: String },
    Enable {
        tool: String,
        #[arg(long)]
        owner: String,
    },
    Disable {
        tool: String,
        #[arg(long)]
        owner: String,
    },
    /// Approve the pending description and schema change.
    Approve {
        tool: String,
        #[arg(long)]
        owner: String,
    },
    /// Restore the previously approved version.
    Rollback {
        tool: String,
        #[arg(long)]
        owner: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    match cli.command {
        Command::Serve => serve(&config).await,
        Command::Diagnostics => {
            let bundle = sekisho::diagnostics::collect(&config);
            println!("{}", serde_json::to_string_pretty(&bundle)?);
            Ok(())
        }
        Command::Registry { command } => registry_command(&config, command),
    }
}

fn registry_command(config: &Config, command: RegistryCommand) -> Result<()> {
    let (file, audit) = (&config.registry.file, &config.registry.audit_file);
    let edit =
        |action: &str,
         tool: &str,
         owner: &str,
         change: &dyn Fn(&mut Registry) -> Result<(), registry::RegistryError>| {
            registry::owner_edit(file, audit, owner, action, tool, |registry| {
                change(registry)
            })
            .map(|_| println!("{action} {tool}"))
        };
    match command {
        RegistryCommand::List => {
            for (key, entry) in Registry::load(file)?.tools {
                let state = match (entry.enabled, entry.available) {
                    (true, true) => "enabled",
                    (false, true) => "disabled",
                    (_, false) => "unavailable",
                };
                let pending = if entry.pending().is_some() {
                    " (pending change)"
                } else {
                    ""
                };
                println!("{key}\t{state}{pending}");
            }
        }
        RegistryCommand::Show { tool } => {
            let registry = Registry::load(file)?;
            let entry = registry
                .tools
                .get(&tool)
                .with_context(|| format!("unknown tool {tool}"))?;
            let view = serde_json::json!({
                "approved": entry.approved_version(),
                "pending": entry.pending(),
            });
            println!("{}", serde_json::to_string_pretty(&view)?);
        }
        RegistryCommand::Enable { tool, owner } => {
            edit("enable", &tool, &owner, &|r| r.set_enabled(&tool, true))?
        }
        RegistryCommand::Disable { tool, owner } => {
            edit("disable", &tool, &owner, &|r| r.set_enabled(&tool, false))?
        }
        RegistryCommand::Approve { tool, owner } => {
            edit("approve", &tool, &owner, &|r| r.approve(&tool))?
        }
        RegistryCommand::Rollback { tool, owner } => {
            edit("rollback", &tool, &owner, &|r| r.rollback(&tool))?
        }
    }
    Ok(())
}

async fn serve(config: &Config) -> Result<()> {
    let verifier = Verifier::from_config(&config.verifier)?;
    let metrics = Arc::new(sekisho::metrics::Metrics::default());
    let state = Arc::new(AppState {
        verifier,
        metrics: metrics.clone(),
    });
    let shutdown = tokio_util::sync::CancellationToken::new();
    let federation = Arc::new(Federation::new(
        config.upstreams.clone(),
        config.registry.file.clone(),
    ));
    tokio::spawn(federation.clone().run(
        Duration::from_millis(config.registry.refresh_interval_ms),
        shutdown.clone(),
    ));
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    println!("listening on {}", listener.local_addr()?);
    let gateway = sekisho::gateway::Gateway::new(
        federation,
        config.policy.clone(),
        sekisho::exchange::Exchanger::new(config.exchange.clone()),
        sekisho::audit::CallAudit::new(config.audit.calls_file.clone()),
        metrics,
    )
    .progressive(config.mcp.progressive);
    let app = router(state, &config.mcp, gateway, shutdown.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}
