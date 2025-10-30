mod config;
mod pack;
mod runner;
mod telemetry;
mod verify;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal;

use crate::config::HostConfig;
use crate::pack::PackRuntime;
use crate::runner::HostServer;

#[derive(Debug, Parser)]
#[command(name = "greentic-runner")]
struct Cli {
    /// Path to the pack.wasm component that exposes pack-export
    #[arg(long)]
    pack: PathBuf,

    /// Bindings yaml describing tenant configuration
    #[arg(long)]
    bindings: PathBuf,

    /// Port to serve the HTTP server on (default 8080)
    #[arg(long, default_value = "8080")]
    port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let host_config = Arc::new(
        HostConfig::load_from_path(&cli.bindings).context("failed to load host bindings")?,
    );

    telemetry::init(&host_config)?;
    tracing::info!(
        tenant = %host_config.tenant,
        bindings_path = %host_config.bindings_path.display(),
        http_enabled = host_config.http_enabled,
        "loaded host configuration"
    );
    tracing::info!(
        messaging_qps = host_config.rate_limits.messaging_send_qps,
        messaging_burst = host_config.rate_limits.messaging_burst,
        "rate limits configured"
    );
    tracing::debug!(
        ?host_config.mcp.store,
        ?host_config.mcp.runtime,
        ?host_config.mcp.security,
        "mcp configuration"
    );
    if let Some(binding) = host_config.messaging_binding() {
        tracing::info!(adapter = %binding.adapter, "messaging adapter configured");
    }

    let pack = Arc::new(
        PackRuntime::load(&cli.pack, Arc::clone(&host_config))
            .await
            .with_context(|| format!("failed to load pack {:?}", cli.pack))?,
    );

    let server = HostServer::new(host_config, pack, cli.port).await?;

    tokio::select! {
        result = server.serve() => {
            result?;
        }
        _ = signal::ctrl_c() => {
            tracing::info!("received shutdown signal");
        }
    }

    Ok(())
}
