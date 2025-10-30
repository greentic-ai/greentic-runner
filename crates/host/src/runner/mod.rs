pub mod adapt_messaging;
pub mod adapt_timer;
pub mod adapt_webhook;
pub mod engine;

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::http::StatusCode;
use axum::routing::{any, post};
use axum::{serve, Router};
use lru::LruCache;
use parking_lot::Mutex;
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use serde_json::Value;

use crate::config::HostConfig;
use crate::pack::PackRuntime;

pub struct HostServer {
    addr: SocketAddr,
    router: Router,
    config: Arc<HostConfig>,
    engine: Arc<engine::FlowEngine>,
    #[allow(dead_code)]
    timer_handles: Vec<JoinHandle<()>>,
}

impl HostServer {
    pub async fn new(config: Arc<HostConfig>, pack: Arc<PackRuntime>, port: u16) -> Result<Self> {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let http_client = Client::builder().build()?;
        let engine = Arc::new(
            engine::FlowEngine::new(Arc::clone(&pack))
                .await
                .context("failed to prime flow engine")?,
        );
        let telegram_capacity = NonZeroUsize::new(TELEGRAM_CACHE_CAPACITY)
            .expect("telegram cache capacity must be > 0");
        let webhook_capacity =
            NonZeroUsize::new(WEBHOOK_CACHE_CAPACITY).expect("webhook cache capacity must be > 0");

        let state = Arc::new(ServerState {
            config: Arc::clone(&config),
            engine: Arc::clone(&engine),
            telegram_cache: Mutex::new(LruCache::new(telegram_capacity)),
            webhook_cache: Mutex::new(LruCache::new(webhook_capacity)),
            http_client,
        });
        let router = Router::new()
            .route(
                "/messaging/telegram/webhook",
                post(adapt_messaging::telegram_webhook),
            )
            .route("/webhook/:flow_id", any(adapt_webhook::dispatch))
            .with_state(Arc::clone(&state));

        let timer_handles =
            adapt_timer::spawn_timers(state).context("failed to spawn timer tasks")?;

        Ok(Self {
            addr,
            router,
            config,
            engine,
            timer_handles,
        })
    }

    pub async fn serve(self) -> Result<()> {
        tracing::info!(
            addr = %self.addr,
            tenant = %self.config.tenant,
            flows = self.engine.flows().len(),
            timers = self.timer_handles.len(),
            "starting host server"
        );
        let listener = TcpListener::bind(self.addr).await?;
        serve(listener, self.router.into_make_service()).await?;
        Ok(())
    }
}

pub struct ServerState {
    pub config: Arc<HostConfig>,
    pub engine: Arc<engine::FlowEngine>,
    pub telegram_cache: Mutex<LruCache<i64, StatusCode>>,
    pub webhook_cache: Mutex<LruCache<String, Value>>,
    pub http_client: Client,
}

impl ServerState {
    pub fn get_secret(&self, key: &str) -> anyhow::Result<String> {
        if !self.config.secrets_policy.is_allowed(key) {
            bail!("secret {key} is not permitted by bindings policy");
        }

        if let Ok(value) = std::env::var(key) {
            return Ok(value);
        }

        bail!("secret {key} not found in environment");
    }
}

const TELEGRAM_CACHE_CAPACITY: usize = 1024;
const WEBHOOK_CACHE_CAPACITY: usize = 256;
