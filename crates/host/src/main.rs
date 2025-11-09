use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use tokio::signal;

use greentic_runner::config::HostConfig;
use greentic_runner::{HostBuilder, HostServer};

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

#[greentic_types::telemetry::main(service_name = "greentic-runner")]
async fn main() {
    if let Err(err) = run().await {
        tracing::error!(error = %err, "runner failed");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let host_config =
        HostConfig::load_from_path(&cli.bindings).context("failed to load host bindings")?;
    let tenant_id = host_config.tenant.clone();

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

    let host = HostBuilder::new().with_config(host_config).build()?;
    host.start().await?;
    host.load_pack(&tenant_id, &cli.pack)
        .await
        .with_context(|| format!("failed to load pack {:?}", cli.pack))?;
    let tenant_handle = host
        .tenant(&tenant_id)
        .await
        .context("tenant handle missing after load")?;

    let server = HostServer::new(tenant_handle.config(), tenant_handle.pack(), cli.port).await?;

    tokio::select! {
        result = server.serve() => {
            result?;
        }
        _ = signal::ctrl_c() => {
            tracing::info!("received shutdown signal");
        }
    }

    host.stop().await?;

    Ok(())
}
