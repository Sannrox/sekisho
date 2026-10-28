//! `sekisho serve` runs the gateway; `sekisho registry` edits the tool
//! registry; `sekisho mitigation` applies and lifts mitigations;
//! `sekisho policy` imports and exports the agent policy; `sekisho state
//! import` copies state between backends; `sekisho diagnostics` prints a
//! support bundle.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use sekisho::config::Config;
use sekisho::gateway::Federation;
use sekisho::registry;
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
    /// Serve `/mcp`, its OAuth metadata, `/healthz`, `/readyz` and `/metrics`. Prints
    /// `listening on <addr>` once bound.
    Serve,
    /// Print a JSON support bundle with no secrets.
    Diagnostics,
    /// Review and change registry tools. Edits are audited.
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
    /// Disable a tool or upstream, or deny a destination, under a decision
    /// reference. Changes are audited and apply to the next call on every
    /// replica.
    Mitigation {
        #[command(subcommand)]
        command: MitigationCommand,
    },
    /// Read or replace the agent policy in the state backend.
    Policy {
        #[command(subcommand)]
        command: PolicyCommand,
    },
    /// Copy state between backends.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
}

#[derive(Subcommand)]
enum PolicyCommand {
    /// Print the stored policy as TOML (the `agents.toml` format).
    Export,
    /// Validate a policy file and replace the stored policy with it. The
    /// next call on every replica uses it.
    Import { file: PathBuf },
}

#[derive(Subcommand)]
enum StateCommand {
    /// Copy policy, registry and mitigations from the backend another
    /// config selects (for example the directory backend being replaced)
    /// into this config's backend. Audit logs stay where they are.
    Import {
        /// Config whose backend holds the state to copy.
        #[arg(long)]
        from: PathBuf,
        /// Recorded as the owner and operator of the imported state.
        #[arg(long)]
        operator: String,
    },
    /// Initialize an empty PostgreSQL store for a new deployment: no
    /// agents, tools or mitigations. Import a policy next.
    Init {
        /// Recorded as the operator who initialized the store.
        #[arg(long)]
        operator: String,
    },
}

#[derive(Subcommand)]
enum MitigationCommand {
    /// Print stored mitigations as JSON, by id.
    List,
    /// Apply a mitigation; prints its id.
    Apply {
        #[command(flatten)]
        target: TargetArgs,
        /// Limit to one tenant (`tenant_id` claim).
        #[arg(long, conflicts_with = "agent")]
        tenant: Option<String>,
        /// Limit to one agent.
        #[arg(long)]
        agent: Option<String>,
        /// Lift automatically after this many seconds.
        #[arg(long)]
        expires_in_secs: Option<u64>,
        #[arg(long)]
        decision: String,
        #[arg(long)]
        operator: String,
    },
    /// Lift a mitigation by id.
    Lift {
        id: String,
        #[arg(long)]
        decision: String,
        #[arg(long)]
        operator: String,
    },
}

#[derive(clap::Args)]
#[group(required = true, multiple = false)]
struct TargetArgs {
    /// Registry key `<upstream>.<tool>`.
    #[arg(long)]
    tool: Option<String>,
    #[arg(long)]
    upstream: Option<String>,
    /// `scheme://host[:port]` an upstream is reached at.
    #[arg(long)]
    destination: Option<String>,
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
        // Store commands block; a PostgreSQL client must not run on an
        // async worker.
        command => {
            tokio::task::spawn_blocking(move || match command {
                Command::Registry { command } => registry_command(&config, command),
                Command::Mitigation { command } => mitigation_command(&config, command),
                Command::Policy { command } => policy_command(&config, command),
                Command::State { command } => state_command(&config, command),
                Command::Serve | Command::Diagnostics => unreachable!("handled above"),
            })
            .await?
        }
    }
}

fn policy_command(config: &Config, command: PolicyCommand) -> Result<()> {
    let store = sekisho::state::open(config)?;
    match command {
        PolicyCommand::Export => print!("{}", toml::to_string(&store.policy()?)?),
        PolicyCommand::Import { file } => {
            let policy = sekisho::policy::PolicyFile::load(&file)?;
            store.set_policy(&policy)?;
            println!("imported {} agents", policy.agents.len());
        }
    }
    Ok(())
}

fn state_command(config: &Config, command: StateCommand) -> Result<()> {
    let target = sekisho::state::open(config)?;
    match command {
        StateCommand::Import { from, operator } => {
            let source = sekisho::state::open(&Config::load(&from)?)?;
            let imported = sekisho::state::import(source.as_ref(), target.as_ref(), &operator)?;
            println!(
                "imported {} agents, {} tools, {} mitigations",
                imported.agents, imported.tools, imported.mitigations
            );
        }
        StateCommand::Init { operator } => {
            sekisho::state::init(target.as_ref(), &operator)?;
            println!("initialized; import a policy with `sekisho policy import <file>`");
        }
    }
    Ok(())
}

fn mitigation_command(config: &Config, command: MitigationCommand) -> Result<()> {
    use sekisho::mitigations::{
        Mitigation, MitigationStore, Outcome, Scope, Standalone, Target, now_ms,
    };
    let store = MitigationStore::new(sekisho::state::open(config)?);
    let now = now_ms();
    let outcome = match command {
        MitigationCommand::List => {
            let state = store.load()?;
            println!("{}", serde_json::to_string_pretty(&state)?);
            return Ok(());
        }
        MitigationCommand::Apply {
            target,
            tenant,
            agent,
            expires_in_secs,
            decision,
            operator,
        } => {
            let target = match (target.tool, target.upstream, target.destination) {
                (Some(tool), _, _) => Target::Tool { tool },
                (_, Some(upstream), _) => Target::Upstream { upstream },
                (_, _, Some(destination)) => Target::Destination { destination },
                _ => unreachable!("clap requires one target"),
            };
            let scope = match (tenant, agent) {
                (Some(tenant), _) => Scope::Tenant { tenant },
                (_, Some(agent)) => Scope::Agent { agent },
                _ => Scope::All,
            };
            let mitigation = Mitigation {
                decision,
                operator,
                target,
                scope,
                applied_at_unix_ms: now,
                expires_at_unix_ms: expires_in_secs
                    .map(|secs| now.saturating_add(secs.saturating_mul(1_000))),
            };
            let (id, outcome) = store.apply(&Standalone, mitigation, now)?;
            println!("{id}");
            outcome
        }
        MitigationCommand::Lift {
            id,
            decision,
            operator,
        } => store.lift(&Standalone, &id, &decision, &operator, now)?,
    };
    if outcome == Outcome::Unchanged {
        eprintln!("unchanged");
    }
    Ok(())
}

fn registry_command(config: &Config, command: RegistryCommand) -> Result<()> {
    let store = sekisho::state::open(config)?;
    let edit =
        |action: &str,
         tool: &str,
         owner: &str,
         change: &dyn Fn(&mut registry::Registry) -> Result<(), registry::RegistryError>| {
            registry::owner_edit(store.as_ref(), owner, action, tool, change)
                .map(|_| println!("{action} {tool}"))
        };
    match command {
        RegistryCommand::List => {
            for (key, entry) in store.registry()?.tools {
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
            let registry = store.registry()?;
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
    let store = sekisho::state::open(config)?;
    // Probe once now, waiting at most the store deadline: a PostgreSQL
    // store connects and migrates its schema, and a store that cannot do
    // work shows in the log. Serving starts either way, as with an
    // unreachable store; `/readyz` reports not ready until it recovers.
    let startup = Arc::clone(&store);
    let probe = sekisho::state::blocking(
        sekisho::state::Store::Limits,
        &mut sekisho::state::STORE_DEADLINE.clone(),
        move || Ok(startup.probe()),
    )
    .await;
    match probe {
        Ok(probe) => {
            for (name, result) in probe {
                if let Err(reason) = result {
                    eprintln!("startup: {name} store unavailable: {reason}");
                }
            }
        }
        Err(_) => eprintln!(
            "startup: the state backend did not answer within {:?}; serving while it recovers",
            sekisho::state::STORE_DEADLINE
        ),
    }
    let federation = Arc::new(Federation::new(
        config.upstreams.clone(),
        Arc::clone(&store),
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
        Arc::clone(&store),
        sekisho::exchange::Exchanger::new(config.exchange.clone()),
        sekisho::audit::CallAudit::new(Arc::clone(&store)).readers(config.audit.readers.clone()),
        metrics,
    )
    .progressive(config.mcp.progressive)
    .guardrails(sekisho::guardrails::Guardrails::new(
        config.guardrails.clone(),
    ))
    .approvals(config.approvals.clone())
    .mitigations(sekisho::mitigations::MitigationStore::new(Arc::clone(
        &store,
    )))
    .limits(sekisho::limits::Limiter::from_config(config, &store));
    let app = router(state, &config.mcp, gateway, shutdown.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}
