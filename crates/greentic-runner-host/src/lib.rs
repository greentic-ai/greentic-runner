#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use greentic_secrets::SecretsBackend;
use runner_core::env::PackConfig;
use tokio::signal;

pub mod boot;
pub mod config;
pub mod engine;
pub mod http;
pub mod imports;
pub mod ingress;
pub mod pack;
pub mod routing;
pub mod runner;
pub mod runtime;
pub mod runtime_wasmtime;
pub mod storage;
pub mod telemetry;
pub mod verify;
pub mod watcher;

mod activity;
mod host;

pub use activity::{Activity, ActivityKind};
pub use config::HostConfig;
pub use host::TelemetryCfg;
pub use host::{HostBuilder, RunnerHost, TenantHandle};

pub use greentic_types::{EnvId, FlowId, PackId, TenantCtx, TenantId};

pub use http::auth::AdminAuth;
pub use routing::RoutingConfig;
use routing::TenantRouting;
pub use runner::HostServer;

/// User-facing configuration for running the unified host.
#[derive(Clone)]
pub struct RunnerConfig {
    pub bindings: Vec<PathBuf>,
    pub pack: PackConfig,
    pub port: u16,
    pub refresh_interval: Duration,
    pub routing: RoutingConfig,
    pub admin: AdminAuth,
    pub telemetry: Option<TelemetryCfg>,
    pub secrets_backend: SecretsBackend,
}

impl RunnerConfig {
    /// Build a [`RunnerConfig`] from environment variables and the provided binding files.
    pub fn from_env(bindings: Vec<PathBuf>) -> Result<Self> {
        if bindings.is_empty() {
            anyhow::bail!("at least one bindings file is required");
        }
        let pack = PackConfig::from_env()?;
        let refresh = parse_refresh_interval(std::env::var("PACK_REFRESH_INTERVAL").ok())?;
        let port = std::env::var("PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8080);
        let routing = RoutingConfig::from_env();
        let admin = AdminAuth::from_env();
        let secrets_backend = SecretsBackend::from_env(std::env::var("SECRETS_BACKEND").ok())?;
        Ok(Self {
            bindings,
            pack,
            port,
            refresh_interval: refresh,
            routing,
            admin,
            telemetry: None,
            secrets_backend,
        })
    }

    /// Override the HTTP port used by the host server.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
}

fn parse_refresh_interval(value: Option<String>) -> Result<Duration> {
    let raw = value.unwrap_or_else(|| "30s".into());
    humantime::parse_duration(&raw).map_err(|err| anyhow!("invalid PACK_REFRESH_INTERVAL: {err}"))
}

/// Run the unified Greentic runner host until shutdown.
pub async fn run(cfg: RunnerConfig) -> Result<()> {
    let mut builder = HostBuilder::new();
    for path in &cfg.bindings {
        let host_config = HostConfig::load_from_path(path)
            .with_context(|| format!("failed to load host bindings {}", path.display()))?;
        builder = builder.with_config(host_config);
    }
    #[cfg(feature = "telemetry")]
    if let Some(telemetry) = cfg.telemetry.clone() {
        builder = builder.with_telemetry(telemetry);
    }

    greentic_secrets::init(cfg.secrets_backend)?;

    let host = Arc::new(builder.build()?);
    host.start().await?;

    let (watcher, reload_handle) =
        watcher::start_pack_watcher(Arc::clone(&host), cfg.pack.clone(), cfg.refresh_interval)
            .await?;

    let routing = TenantRouting::new(cfg.routing.clone());
    let server = HostServer::new(
        cfg.port,
        host.active_packs(),
        routing,
        host.health_state(),
        Some(reload_handle),
        cfg.admin.clone(),
    )?;

    tokio::select! {
        result = server.serve() => {
            result?;
        }
        _ = signal::ctrl_c() => {
            tracing::info!("received shutdown signal");
        }
    }

    drop(watcher);
    host.stop().await?;
    Ok(())
}
