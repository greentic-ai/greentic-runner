#![deny(unsafe_code)]
//! Official host runtime for Greentic packs and flows.
//! `greentic-runner` embeds this crate as the one true host; the older `greentic-host`
//! crate/docs are deprecated. Timer/cron flows are intentionally absent here and will be
//! handled by a future `greentic-events` project so the runner can focus on sessionful
//! messaging and webhooks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use greentic_secrets::SecretsBackend;
use runner_core::env::PackConfig;
use tokio::signal;

#[doc(hidden)]
/// Internal bootstrap helpers not part of the stable API.
pub mod boot;
pub mod config;
#[doc(hidden)]
/// Internal engine/runtime glue; prefer `HostBuilder`/`RunnerHost`.
pub mod engine;
#[doc(hidden)]
/// HTTP helpers primarily used by the host server.
pub mod http;
#[doc(hidden)]
/// Low-level import/wasi helpers maintained for runtime wiring.
pub mod imports;
#[doc(hidden)]
/// Canonical ingress adapters (messaging flows, webhooks, etc.).
pub mod ingress;
#[doc(hidden)]
/// Pack loading helpers and runtime embeddings.
pub mod pack;
pub mod routing;
#[doc(hidden)]
/// HTTP server wiring; use [`HostServer`] instead.
pub mod runner;
#[doc(hidden)]
/// Tenant runtime internals (engine/state machine) not part of the public API.
pub mod runtime;
#[doc(hidden)]
/// Wasmtime-specific bindings used internally.
pub mod runtime_wasmtime;
#[doc(hidden)]
/// Storage/backing stores for secrets/state.
pub mod storage;
#[doc(hidden)]
/// Telemetry helpers used by the host.
pub mod telemetry;
#[doc(hidden)]
/// Verification helpers for pack/signature checks.
pub mod verify;
#[doc(hidden)]
/// WASI helpers for the runner.
pub mod wasi;
#[doc(hidden)]
/// Watcher helpers that drive pack reloading.
pub mod watcher;

mod activity;
mod host;

pub use activity::{Activity, ActivityKind};
pub use config::HostConfig;
pub use host::TelemetryCfg;
pub use host::{HostBuilder, RunnerHost, TenantHandle};
pub use wasi::{PreopenSpec, RunnerWasiPolicy};

pub use greentic_types::{EnvId, FlowId, PackId, TenantCtx, TenantId};

pub use http::auth::AdminAuth;
pub use routing::RoutingConfig;
use routing::TenantRouting;
pub use runner::HostServer;

/// Configuration for the canonical Greentic host binary (bindings + pack + routing + admin).
///
/// This struct exposes the official entrypoint for new integrations and enforces the
/// canonical environment/tenant/session invariants relied upon by `HostBuilder`s and
/// `RunnerHost`.
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
    pub wasi_policy: RunnerWasiPolicy,
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
            wasi_policy: RunnerWasiPolicy::default(),
        })
    }

    /// Override the HTTP port used by the host server.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_wasi_policy(mut self, policy: RunnerWasiPolicy) -> Self {
        self.wasi_policy = policy;
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
    builder = builder.with_wasi_policy(cfg.wasi_policy.clone());

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
