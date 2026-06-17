pub mod adapt_events_email;
pub mod adapt_timer;
pub mod agent_node;
pub mod contract_cache;
pub mod contract_introspection;
pub mod dispatch_listener;
pub mod engine;
pub mod flow_adapter;
pub mod graph_node;
pub mod i18n;
pub mod invocation;
pub mod mcp_node;
pub mod mocks;
pub mod operator;
pub mod remote_dispatch;
pub mod runtime_session_resumer;
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
use crate::sql::SqlGateway;
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
        Self::with_sql(port, active, routing, health, reload, admin, None)
    }

    /// Like [`HostServer::new`] but wires an optional [`SqlGateway`] into the
    /// server state.  When `sql_gateway` is `None` the server uses an empty
    /// gateway that returns 404/401 for all `/sql` routes — safe by default.
    pub fn with_sql(
        port: u16,
        active: Arc<ActivePacks>,
        routing: TenantRouting,
        health: Arc<HealthState>,
        reload: Option<PackReloadHandle>,
        admin: AdminAuth,
        sql_gateway: Option<SqlGateway>,
    ) -> Result<Self> {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let sql = sql_gateway
            .unwrap_or_else(|| SqlGateway::new(std::collections::HashMap::new(), String::new()));
        let state = ServerState {
            active,
            routing,
            health,
            reload,
            admin,
            sql,
        };
        let router = Router::new()
            .route("/operator/op/invoke", post(operator::invoke))
            .route("/healthz", get(http::health::handler))
            .route("/admin/packs/status", get(admin::status))
            .route("/admin/packs/reload", post(admin::reload))
            .route(
                "/sql/{conn}/schema",
                get(crate::sql::routes::schema_handler),
            )
            .route("/sql/{conn}/query", post(crate::sql::routes::query_handler))
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
    /// SQL gateway for `/sql/{conn}/schema` and `/sql/{conn}/query`.
    /// Defaults to an empty gateway (returns 404/401 for all connections)
    /// when no SQL connections are configured.
    pub sql: SqlGateway,
}

impl axum::extract::FromRef<ServerState> for SqlGateway {
    fn from_ref(state: &ServerState) -> Self {
        state.sql.clone()
    }
}
