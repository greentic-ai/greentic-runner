#![cfg(feature = "new-runner")]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use greentic_runner::glue::{FnSecretsHost, FnTelemetryHost};
use greentic_runner::newrunner::builder::RunnerBuilder;
use greentic_runner::newrunner::policy::Policy;
use greentic_runner::newrunner::shims::{InMemorySessionHost, InMemoryStateHost};
use greentic_runner::newrunner::{
    Runner, RunnerApi, RunnerError, api::RunFlowRequest, host::HostBundle,
    registry::AdapterRegistry, state_machine::FlowDefinition,
};
use greentic_types::{EnvId, TenantCtx, TenantId};
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "greentic-runner-cli")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// List flows described in the given manifest file.
    ListFlows {
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Retrieve the JSON schema for a flow by id.
    GetFlowSchema {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        flow_id: String,
    },
    /// Execute a flow once with the provided input and tenant context.
    RunFlow {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        flow_id: String,
        #[arg(long)]
        tenant: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        session_hint: Option<String>,
    },
}

#[greentic_types::telemetry::main(service_name = "greentic-runner-cli")]
async fn main() {
    if let Err(err) = run_cli().await {
        eprintln!("greentic-runner-cli: {err:?}");
        std::process::exit(1);
    }
}

async fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::ListFlows { manifest } => {
            let runner = build_runner(&manifest).await?;
            let tenant = default_tenant();
            let flows = runner.list_flows(&tenant).await?;
            println!("{}", serde_json::to_string_pretty(&flows)?);
        }
        Commands::GetFlowSchema { manifest, flow_id } => {
            let runner = build_runner(&manifest).await?;
            let tenant = default_tenant();
            let schema = runner.get_flow_schema(&tenant, &flow_id).await?;
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        Commands::RunFlow {
            manifest,
            flow_id,
            tenant,
            input,
            session_hint,
        } => {
            let runner = build_runner(&manifest).await?;
            let tenant_ctx = serde_json::from_str::<TenantCtx>(&tenant)
                .context("failed to parse tenant context json")?;
            let input_json: Value =
                serde_json::from_str(&input).context("failed to parse input json")?;
            let result = runner
                .run_flow(RunFlowRequest {
                    tenant: tenant_ctx,
                    flow_id,
                    input: input_json,
                    session_hint,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }
    Ok(())
}

async fn build_runner(manifest: &PathBuf) -> anyhow::Result<Runner> {
    let flows = load_flows(manifest).await?;
    let secrets = Arc::new(FnSecretsHost::new(|name| {
        std::env::var(name).map_err(|err| RunnerError::Secrets {
            reason: err.to_string(),
        })
    }));
    let telemetry = Arc::new(FnTelemetryHost::new(|span, fields| {
        for (k, v) in fields {
            tracing::debug!(trace_id = ?span.trace_id, span_id = ?span.span_id, key = *k, value = *v, "telemetry");
        }
        Ok(())
    }));
    let session = Arc::new(InMemorySessionHost::new());
    let state = Arc::new(InMemoryStateHost::new());
    let host = HostBundle::new(secrets, telemetry, session, state);
    let mut builder = RunnerBuilder::new()
        .with_host(host)
        .with_adapters(AdapterRegistry::default())
        .with_policy(Policy::default());
    for flow in flows {
        builder = builder.with_flow(flow);
    }
    Ok(builder.build()?)
}

async fn load_flows(path: &PathBuf) -> anyhow::Result<Vec<FlowDefinition>> {
    let bytes = tokio::fs::read(path)
        .await
        .context("failed to read manifest")?;
    let flows = serde_json::from_slice::<Vec<FlowDefinition>>(&bytes)
        .context("failed to parse manifest json")?;
    Ok(flows)
}

fn default_tenant() -> TenantCtx {
    TenantCtx::new(EnvId::from("cli"), TenantId::from("local"))
}
