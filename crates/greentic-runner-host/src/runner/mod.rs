pub mod adapt_events_email;
pub mod adapt_timer;
pub mod agent_node;
pub mod contract_cache;
pub mod contract_introspection;
pub mod engine;
pub mod flow_adapter;
pub mod i18n;
pub mod invocation;
pub mod mocks;
pub mod operator;
pub mod schema_validator;
pub mod templating;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::routing::{get, post};
use axum::{Router, serve};
use tokio::net::TcpListener;

use crate::http::{self, admin, auth::AdminAuth, health::HealthState};
use crate::routing::TenantRouting;
use crate::runtime::ActivePacks;
use crate::watcher::PackReloadHandle;

pub struct HostServer {
    addr: SocketAddr,
    router: Router,
    _state: ServerState,
}

impl HostServer {
    pub fn new(
        port: u16,
        active: Arc<ActivePacks>,
        routing: TenantRouting,
        health: Arc<HealthState>,
        reload: Option<PackReloadHandle>,
        admin: AdminAuth,
    ) -> Result<Self> {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let state = ServerState {
            active,
            routing,
            health,
            reload,
            admin,
        };
        let router = Router::new()
            .route("/operator/op/invoke", post(operator::invoke))
            .route("/healthz", get(http::health::handler))
            .route("/admin/packs/status", get(admin::status))
            .route("/admin/packs/reload", post(admin::reload))
            .with_state(state.clone());
        Ok(Self {
            addr,
            router,
            _state: state,
        })
    }

    pub async fn serve(self) -> Result<()> {
        tracing::info!(addr = %self.addr, "starting host server");
        let listener = TcpListener::bind(self.addr).await?;
        serve(
            listener,
            self.router
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct ServerState {
    pub active: Arc<ActivePacks>,
    pub routing: TenantRouting,
    pub health: Arc<HealthState>,
    pub reload: Option<PackReloadHandle>,
    pub admin: AdminAuth,
}
