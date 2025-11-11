use clap::Parser;
use greentic_runner_host::{RunnerConfig, run as run_host};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "greentic-runner")]
struct Cli {
    /// Bindings yaml describing tenant configuration (repeat per tenant)
    #[arg(long = "bindings", value_name = "PATH", required = true)]
    bindings: Vec<PathBuf>,

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
    let cfg = RunnerConfig::from_env(cli.bindings)?.with_port(cli.port);
    run_host(cfg).await
}
