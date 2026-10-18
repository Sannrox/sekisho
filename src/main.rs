//! `sekisho serve` runs the gateway; `sekisho diagnostics` prints a support bundle.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use sekisho::config::Config;
use sekisho::server::{AppState, router};
use sekisho::verifier::Verifier;

#[derive(Parser)]
#[command(
    name = "sekisho",
    version,
    about = "Governed tool gateway for AI agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve `/mcp`, its OAuth metadata, `/healthz` and `/metrics`. Prints `listening on <addr>` once bound.
    Serve {
        #[arg(long, default_value = "sekisho.toml")]
        config: PathBuf,
    },
    /// Print a JSON support bundle with no secrets.
    Diagnostics {
        #[arg(long, default_value = "sekisho.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Serve { config } => serve(&Config::load(&config)?).await,
        Command::Diagnostics { config } => {
            let bundle = sekisho::diagnostics::collect(&Config::load(&config)?);
            println!("{}", serde_json::to_string_pretty(&bundle)?);
            Ok(())
        }
    }
}

async fn serve(config: &Config) -> Result<()> {
    let verifier = Verifier::from_config(&config.verifier)?;
    let state = Arc::new(AppState {
        verifier,
        metrics: Default::default(),
    });
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    println!("listening on {}", listener.local_addr()?);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let app = router(state, &config.mcp, shutdown.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}
