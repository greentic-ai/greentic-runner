use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use crate::runtime_wasmtime::{Component, Engine, Linker, ResourceTable, Store, WasmResult};
use anyhow::{Context, Result, anyhow, bail};
use greentic_flow::ir::{FlowIR, NodeIR, RouteIR};
use greentic_interfaces::host_import_v0_2;
use greentic_interfaces::host_import_v0_2::greentic::host_import::imports::{
    HttpRequest as LegacyHttpRequest, HttpResponse as LegacyHttpResponse,
    IfaceError as LegacyIfaceError, TenantCtx as LegacyTenantCtx,
};
use greentic_interfaces::host_import_v0_6::{self, iface_types, state, types};
use greentic_interfaces::pack_export_v0_2;
use greentic_interfaces::pack_export_v0_2::exports::greentic::pack_export::exports::FlowInfo;
#[cfg(feature = "mcp")]
use greentic_mcp::{ExecConfig, ExecError, ExecRequest};
use greentic_session::SessionKey as StoreSessionKey;
use greentic_types::{
    EnvId, FlowId, SessionCursor as StoreSessionCursor, SessionData, StateKey as StoreStateKey,
    TeamId, TenantCtx as TypesTenantCtx, TenantId, UserId,
};
use indexmap::IndexMap;
use reqwest::blocking::Client as BlockingClient;
use serde::{Deserialize, Serialize};
use serde_cbor;
use serde_json::{self, Value, json};
use serde_yaml_bw as serde_yaml;
use tokio::fs;
use wasmparser::{Parser, Payload};
use zip::ZipArchive;

use crate::imports;
use crate::runner::mocks::{HttpDecision, HttpMockRequest, HttpMockResponse, MockLayer};

use crate::config::HostConfig;
use crate::storage::state::STATE_PREFIX;
use crate::storage::{DynSessionStore, DynStateStore};
use crate::verify;
use crate::wasi::RunnerWasiPolicy;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

pub struct PackRuntime {
    path: PathBuf,
    config: Arc<HostConfig>,
    engine: Engine,
    component: Option<Component>,
    metadata: PackMetadata,
    mocks: Option<Arc<MockLayer>>,
    archive: Option<ArchiveFlows>,
    session_store: Option<DynSessionStore>,
    state_store: Option<DynStateStore>,
    wasi_policy: Arc<RunnerWasiPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDescriptor {
    pub id: String,
    #[serde(rename = "type")]
    pub flow_type: String,
    pub profile: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
}

pub struct HostState {
    config: Arc<HostConfig>,
    http_client: BlockingClient,
    #[cfg(feature = "mcp")]
    exec_config: Option<ExecConfig>,
    default_env: String,
    session_store: Option<DynSessionStore>,
    state_store: Option<DynStateStore>,
    mocks: Option<Arc<MockLayer>>,
}

impl HostState {
    pub fn new(
        config: Arc<HostConfig>,
        mocks: Option<Arc<MockLayer>>,
        session_store: Option<DynSessionStore>,
        state_store: Option<DynStateStore>,
    ) -> Result<Self> {
        let http_client = BlockingClient::builder().build()?;
        #[cfg(feature = "mcp")]
        let exec_config = config.mcp_exec_config().ok();
        let default_env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
        Ok(Self {
            config,
            http_client,
            #[cfg(feature = "mcp")]
            exec_config,
            default_env,
            session_store,
            state_store,
            mocks,
        })
    }

    pub fn get_secret(&self, key: &str) -> Result<String> {
        if !self.config.secrets_policy.is_allowed(key) {
            bail!("secret {key} is not permitted by bindings policy");
        }
        if let Some(mock) = &self.mocks
            && let Some(value) = mock.secrets_lookup(key)
        {
            return Ok(value);
        }
        if let Ok(value) = std::env::var(key) {
            return Ok(value);
        }
        bail!("secret {key} not found in environment");
    }

    fn tenant_ctx_from_v6(&self, ctx: Option<types::TenantCtx>) -> Result<TypesTenantCtx> {
        let tenant_raw = ctx
            .as_ref()
            .map(|ctx| ctx.tenant.clone())
            .unwrap_or_else(|| self.config.tenant.clone());
        let tenant_id = TenantId::from_str(&tenant_raw)
            .with_context(|| format!("invalid tenant id `{tenant_raw}`"))?;
        let env_id = EnvId::from_str(&self.default_env)
            .unwrap_or_else(|_| EnvId::from_str("local").expect("default env must be valid"));
        let mut tenant_ctx = TypesTenantCtx::new(env_id, tenant_id);
        if let Some(ctx) = ctx {
            if let Some(team) = ctx.team {
                let team_id =
                    TeamId::from_str(&team).with_context(|| format!("invalid team id `{team}`"))?;
                tenant_ctx = tenant_ctx.with_team(Some(team_id));
            }
            if let Some(user) = ctx.user {
                let user_id =
                    UserId::from_str(&user).with_context(|| format!("invalid user id `{user}`"))?;
                tenant_ctx = tenant_ctx.with_user(Some(user_id));
            }
            if let Some(flow) = ctx.flow_id {
                tenant_ctx = tenant_ctx.with_flow(flow);
            }
            if let Some(node) = ctx.node_id {
                tenant_ctx = tenant_ctx.with_node(node);
            }
            if let Some(provider) = ctx.provider_id {
                tenant_ctx = tenant_ctx.with_provider(provider);
            }
            if let Some(session) = ctx.session_id {
                tenant_ctx = tenant_ctx.with_session(session);
            }
            tenant_ctx.trace_id = ctx.trace_id;
        }
        Ok(tenant_ctx)
    }

    fn session_store_handle(&self) -> Result<DynSessionStore, types::IfaceError> {
        self.session_store
            .as_ref()
            .cloned()
            .ok_or(types::IfaceError::Unavailable)
    }

    fn state_store_handle(&self) -> Result<DynStateStore, types::IfaceError> {
        self.state_store
            .as_ref()
            .cloned()
            .ok_or(types::IfaceError::Unavailable)
    }

    fn ensure_user(ctx: &TypesTenantCtx) -> Result<UserId, types::IfaceError> {
        ctx.user_id
            .clone()
            .or_else(|| ctx.user.clone())
            .ok_or(types::IfaceError::InvalidArg)
    }

    fn ensure_flow(ctx: &TypesTenantCtx) -> Result<FlowId, types::IfaceError> {
        let flow = ctx.flow_id().ok_or(types::IfaceError::InvalidArg)?;
        FlowId::from_str(flow).map_err(|_| types::IfaceError::InvalidArg)
    }

    fn cursor_from_iface(cursor: iface_types::SessionCursor) -> StoreSessionCursor {
        let mut store_cursor = StoreSessionCursor::new(cursor.node_pointer);
        if let Some(reason) = cursor.wait_reason {
            store_cursor = store_cursor.with_wait_reason(reason);
        }
        if let Some(marker) = cursor.outbox_marker {
            store_cursor = store_cursor.with_outbox_marker(marker);
        }
        store_cursor
    }
}

pub struct ComponentState {
    host: HostState,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

impl ComponentState {
    pub fn new(host: HostState, policy: Arc<RunnerWasiPolicy>) -> Result<Self> {
        let wasi_ctx = policy
            .instantiate()
            .context("failed to build WASI context")?;
        Ok(Self {
            host,
            wasi_ctx,
            resource_table: ResourceTable::new(),
        })
    }

    fn host_mut(&mut self) -> &mut HostState {
        &mut self.host
    }
}

impl WasiView for ComponentState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resource_table,
        }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for ComponentState {}
#[allow(unsafe_code)]
unsafe impl Sync for ComponentState {}

impl host_import_v0_6::HostImports for ComponentState {
    fn secrets_get(
        &mut self,
        key: String,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        host_import_v0_6::HostImports::secrets_get(self.host_mut(), key, ctx)
    }

    fn telemetry_emit(
        &mut self,
        span_json: String,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<()> {
        host_import_v0_6::HostImports::telemetry_emit(self.host_mut(), span_json, ctx)
    }

    fn http_fetch(
        &mut self,
        req: host_import_v0_6::http::HttpRequest,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<host_import_v0_6::http::HttpResponse, types::IfaceError>> {
        host_import_v0_6::HostImports::http_fetch(self.host_mut(), req, ctx)
    }

    fn mcp_exec(
        &mut self,
        component: String,
        action: String,
        args_json: String,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        host_import_v0_6::HostImports::mcp_exec(self.host_mut(), component, action, args_json, ctx)
    }

    fn state_get(
        &mut self,
        key: iface_types::StateKey,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        host_import_v0_6::HostImports::state_get(self.host_mut(), key, ctx)
    }

    fn state_set(
        &mut self,
        key: iface_types::StateKey,
        value_json: String,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<state::OpAck, types::IfaceError>> {
        host_import_v0_6::HostImports::state_set(self.host_mut(), key, value_json, ctx)
    }

    fn session_update(
        &mut self,
        cursor: iface_types::SessionCursor,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        host_import_v0_6::HostImports::session_update(self.host_mut(), cursor, ctx)
    }
}

impl host_import_v0_2::HostImports for ComponentState {
    fn secrets_get(
        &mut self,
        key: String,
        ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<Result<String, LegacyIfaceError>> {
        host_import_v0_2::HostImports::secrets_get(self.host_mut(), key, ctx)
    }

    fn telemetry_emit(
        &mut self,
        span_json: String,
        ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<()> {
        host_import_v0_2::HostImports::telemetry_emit(self.host_mut(), span_json, ctx)
    }

    fn tool_invoke(
        &mut self,
        tool: String,
        action: String,
        args_json: String,
        ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<Result<String, LegacyIfaceError>> {
        host_import_v0_2::HostImports::tool_invoke(self.host_mut(), tool, action, args_json, ctx)
    }

    fn http_fetch(
        &mut self,
        req: host_import_v0_2::greentic::host_import::imports::HttpRequest,
        ctx: Option<host_import_v0_2::greentic::host_import::imports::TenantCtx>,
    ) -> WasmResult<
        Result<
            host_import_v0_2::greentic::host_import::imports::HttpResponse,
            host_import_v0_2::greentic::host_import::imports::IfaceError,
        >,
    > {
        host_import_v0_2::HostImports::http_fetch(self.host_mut(), req, ctx)
    }
}

impl host_import_v0_6::HostImports for HostState {
    fn secrets_get(
        &mut self,
        key: String,
        _ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        Ok(self.get_secret(&key).map_err(|err| {
            tracing::warn!(secret = %key, error = %err, "secret lookup denied");
            types::IfaceError::Denied
        }))
    }

    fn telemetry_emit(
        &mut self,
        span_json: String,
        _ctx: Option<types::TenantCtx>,
    ) -> WasmResult<()> {
        if let Some(mock) = &self.mocks
            && mock.telemetry_drain(&[("span_json", span_json.as_str())])
        {
            return Ok(());
        }
        tracing::info!(span = %span_json, "telemetry emit from pack");
        Ok(())
    }

    fn http_fetch(
        &mut self,
        req: host_import_v0_6::http::HttpRequest,
        _ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<host_import_v0_6::http::HttpResponse, types::IfaceError>> {
        let legacy_req = LegacyHttpRequest {
            method: req.method,
            url: req.url,
            headers_json: req.headers_json,
            body: req.body,
        };
        match greentic_interfaces::host_import_v0_2::HostImports::http_fetch(
            self, legacy_req, None,
        )? {
            Ok(resp) => Ok(Ok(host_import_v0_6::http::HttpResponse {
                status: resp.status,
                headers_json: resp.headers_json,
                body: resp.body,
            })),
            Err(err) => Ok(Err(map_legacy_error(err))),
        }
    }

    fn mcp_exec(
        &mut self,
        component: String,
        action: String,
        args_json: String,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (component, action, args_json, ctx);
            tracing::warn!("mcp.exec requested but crate built without `mcp` feature");
            return Ok(Err(types::IfaceError::Unavailable));
        }
        #[cfg(feature = "mcp")]
        {
            let exec_config = match &self.exec_config {
                Some(cfg) => cfg.clone(),
                None => {
                    tracing::warn!(component = %component, action = %action, "exec config unavailable for tool invoke");
                    return Ok(Err(types::IfaceError::Unavailable));
                }
            };
            let args: Value = match serde_json::from_str(&args_json) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!(error = %err, "invalid args for mcp.exec node");
                    return Ok(Err(types::IfaceError::InvalidArg));
                }
            };
            let tenant = match self.tenant_ctx_from_v6(ctx) {
                Ok(ctx) => Some(ctx),
                Err(err) => {
                    tracing::warn!(error = %err, "failed to parse tenant context for mcp.exec");
                    None
                }
            };
            let request = ExecRequest {
                component: component.clone(),
                action: action.clone(),
                args,
                tenant,
            };
            match greentic_mcp::exec(request, &exec_config) {
                Ok(value) => match serde_json::to_string(&value) {
                    Ok(body) => Ok(Ok(body)),
                    Err(err) => {
                        tracing::error!(error = %err, "failed to serialise tool result");
                        Ok(Err(types::IfaceError::Internal))
                    }
                },
                Err(err) => {
                    tracing::warn!(component = %component, action = %action, error = %err, "mcp exec failed");
                    let iface_err = match err {
                        ExecError::NotFound { .. } => types::IfaceError::NotFound,
                        ExecError::Tool { .. } => types::IfaceError::Denied,
                        _ => types::IfaceError::Unavailable,
                    };
                    Ok(Err(iface_err))
                }
            }
        }
    }

    fn state_get(
        &mut self,
        key: iface_types::StateKey,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        let store = match self.state_store_handle() {
            Ok(store) => store,
            Err(err) => return Ok(Err(err)),
        };
        let tenant_ctx = match self.tenant_ctx_from_v6(ctx) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::warn!(error = %err, "invalid tenant context for state.get");
                return Ok(Err(types::IfaceError::InvalidArg));
            }
        };
        let key = StoreStateKey::from(key);
        match store.get_json(&tenant_ctx, STATE_PREFIX, &key, None) {
            Ok(Some(value)) => {
                let result = if let Some(text) = value.as_str() {
                    text.to_string()
                } else {
                    serde_json::to_string(&value).unwrap_or_else(|_| value.to_string())
                };
                Ok(Ok(result))
            }
            Ok(None) => Ok(Err(types::IfaceError::NotFound)),
            Err(err) => {
                tracing::warn!(error = %err, "state.get failed");
                Ok(Err(types::IfaceError::Internal))
            }
        }
    }

    fn state_set(
        &mut self,
        key: iface_types::StateKey,
        value_json: String,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<state::OpAck, types::IfaceError>> {
        let store = match self.state_store_handle() {
            Ok(store) => store,
            Err(err) => return Ok(Err(err)),
        };
        let tenant_ctx = match self.tenant_ctx_from_v6(ctx) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::warn!(error = %err, "invalid tenant context for state.set");
                return Ok(Err(types::IfaceError::InvalidArg));
            }
        };
        let key = StoreStateKey::from(key);
        let value = serde_json::from_str(&value_json).unwrap_or(Value::String(value_json));
        match store.set_json(&tenant_ctx, STATE_PREFIX, &key, None, &value, None) {
            Ok(()) => Ok(Ok(state::OpAck::Ok)),
            Err(err) => {
                tracing::warn!(error = %err, "state.set failed");
                Ok(Err(types::IfaceError::Internal))
            }
        }
    }

    fn session_update(
        &mut self,
        cursor: iface_types::SessionCursor,
        ctx: Option<types::TenantCtx>,
    ) -> WasmResult<Result<String, types::IfaceError>> {
        let store = match self.session_store_handle() {
            Ok(store) => store,
            Err(err) => return Ok(Err(err)),
        };
        let tenant_ctx = match self.tenant_ctx_from_v6(ctx) {
            Ok(ctx) => ctx,
            Err(err) => {
                tracing::warn!(error = %err, "invalid tenant context for session.update");
                return Ok(Err(types::IfaceError::InvalidArg));
            }
        };
        let user = match Self::ensure_user(&tenant_ctx) {
            Ok(user) => user,
            Err(err) => return Ok(Err(err)),
        };
        let flow_id = match Self::ensure_flow(&tenant_ctx) {
            Ok(flow) => flow,
            Err(err) => return Ok(Err(err)),
        };
        let cursor = Self::cursor_from_iface(cursor);
        let payload = SessionData {
            tenant_ctx: tenant_ctx.clone(),
            flow_id,
            cursor: cursor.clone(),
            context_json: serde_json::json!({
                "node_pointer": cursor.node_pointer,
                "wait_reason": cursor.wait_reason,
                "outbox_marker": cursor.outbox_marker,
            })
            .to_string(),
        };
        if let Some(existing) = tenant_ctx.session_id() {
            let key = StoreSessionKey::from(existing.to_string());
            if let Err(err) = store.update_session(&key, payload) {
                tracing::error!(error = %err, "failed to update session snapshot");
                return Ok(Err(types::IfaceError::Internal));
            }
            return Ok(Ok(existing.to_string()));
        }
        match store.find_by_user(&tenant_ctx, &user) {
            Ok(Some((key, _))) => {
                if let Err(err) = store.update_session(&key, payload) {
                    tracing::error!(error = %err, "failed to update existing user session");
                    return Ok(Err(types::IfaceError::Internal));
                }
                return Ok(Ok(key.to_string()));
            }
            Ok(None) => {}
            Err(err) => {
                tracing::error!(error = %err, "session lookup failed");
                return Ok(Err(types::IfaceError::Internal));
            }
        }
        let key = match store.create_session(&tenant_ctx, payload.clone()) {
            Ok(key) => key,
            Err(err) => {
                tracing::error!(error = %err, "failed to create session");
                return Ok(Err(types::IfaceError::Internal));
            }
        };
        let ctx_with_session = tenant_ctx.with_session(key.to_string());
        let updated_payload = SessionData {
            tenant_ctx: ctx_with_session.clone(),
            ..payload
        };
        if let Err(err) = store.update_session(&key, updated_payload) {
            tracing::warn!(error = %err, "failed to stamp session id after create");
        }
        Ok(Ok(key.to_string()))
    }
}

impl greentic_interfaces::host_import_v0_2::HostImports for HostState {
    fn secrets_get(
        &mut self,
        key: String,
        _ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<Result<String, LegacyIfaceError>> {
        Ok(self.get_secret(&key).map_err(|err| {
            tracing::warn!(secret = %key, error = %err, "secret lookup denied");
            LegacyIfaceError::Denied
        }))
    }

    fn telemetry_emit(
        &mut self,
        span_json: String,
        _ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<()> {
        if let Some(mock) = &self.mocks
            && mock.telemetry_drain(&[("span_json", span_json.as_str())])
        {
            return Ok(());
        }
        tracing::info!(span = %span_json, "telemetry emit from pack");
        Ok(())
    }

    fn tool_invoke(
        &mut self,
        tool: String,
        action: String,
        args_json: String,
        ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<Result<String, LegacyIfaceError>> {
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (tool, action, args_json, ctx);
            tracing::warn!("tool invoke requested but crate built without `mcp` feature");
            return Ok(Err(LegacyIfaceError::Unavailable));
        }

        #[cfg(feature = "mcp")]
        {
            let exec_config = match &self.exec_config {
                Some(cfg) => cfg.clone(),
                None => {
                    tracing::warn!(%tool, %action, "exec config unavailable for tool invoke");
                    return Ok(Err(LegacyIfaceError::Unavailable));
                }
            };

            let args: Value = match serde_json::from_str(&args_json) {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!(error = %err, "invalid args for tool invoke");
                    return Ok(Err(LegacyIfaceError::InvalidArg));
                }
            };

            let tenant = ctx.map(|ctx| map_legacy_tenant_ctx(ctx, &self.default_env));

            let request = ExecRequest {
                component: tool.clone(),
                action: action.clone(),
                args,
                tenant,
            };

            if let Some(mock) = &self.mocks
                && let Some(result) = mock.tool_short_circuit(&tool, &action)
            {
                return match result.and_then(|value| {
                    serde_json::to_string(&value)
                        .map_err(|err| anyhow!("failed to serialise mock tool output: {err}"))
                }) {
                    Ok(body) => Ok(Ok(body)),
                    Err(err) => {
                        tracing::error!(error = %err, "mock tool execution failed");
                        Ok(Err(LegacyIfaceError::Internal))
                    }
                };
            }

            match greentic_mcp::exec(request, &exec_config) {
                Ok(value) => match serde_json::to_string(&value) {
                    Ok(body) => Ok(Ok(body)),
                    Err(err) => {
                        tracing::error!(error = %err, "failed to serialise tool result");
                        Ok(Err(LegacyIfaceError::Internal))
                    }
                },
                Err(err) => {
                    tracing::warn!(%tool, %action, error = %err, "tool invoke failed");
                    let iface_err = match err {
                        ExecError::NotFound { .. } => LegacyIfaceError::NotFound,
                        ExecError::Tool { .. } => LegacyIfaceError::Denied,
                        _ => LegacyIfaceError::Unavailable,
                    };
                    Ok(Err(iface_err))
                }
            }
        }
    }

    fn http_fetch(
        &mut self,
        req: LegacyHttpRequest,
        _ctx: Option<LegacyTenantCtx>,
    ) -> WasmResult<Result<LegacyHttpResponse, LegacyIfaceError>> {
        if !self.config.http_enabled {
            tracing::warn!(url = %req.url, "http fetch denied by policy");
            return Ok(Err(LegacyIfaceError::Denied));
        }

        let mut mock_state = None;
        let raw_body = req.body.clone();
        if let Some(mock) = &self.mocks
            && let Ok(meta) = HttpMockRequest::new(
                &req.method,
                &req.url,
                raw_body.as_deref().map(|body| body.as_bytes()),
            )
        {
            match mock.http_begin(&meta) {
                HttpDecision::Mock(response) => {
                    let http = LegacyHttpResponse::from(&response);
                    return Ok(Ok(http));
                }
                HttpDecision::Deny(reason) => {
                    tracing::warn!(url = %req.url, reason = %reason, "http fetch blocked by mocks");
                    return Ok(Err(LegacyIfaceError::Denied));
                }
                HttpDecision::Passthrough { record } => {
                    mock_state = Some((meta, record));
                }
            }
        }

        let method = req.method.parse().unwrap_or(reqwest::Method::GET);
        let mut builder = self.http_client.request(method, &req.url);

        if let Some(headers_json) = req.headers_json.as_ref() {
            match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(headers_json) {
                Ok(map) => {
                    for (key, value) in map {
                        if let Some(val) = value.as_str()
                            && let Ok(header) =
                                reqwest::header::HeaderName::from_bytes(key.as_bytes())
                            && let Ok(header_value) = reqwest::header::HeaderValue::from_str(val)
                        {
                            builder = builder.header(header, header_value);
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to parse headers for http.fetch");
                }
            }
        }

        if let Some(body) = raw_body.clone() {
            builder = builder.body(body);
        }

        let response = match builder.send() {
            Ok(resp) => resp,
            Err(err) => {
                tracing::error!(url = %req.url, error = %err, "http fetch failed");
                return Ok(Err(LegacyIfaceError::Unavailable));
            }
        };

        let status = response.status().as_u16();
        let headers_map = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let headers_json = serde_json::to_string(&headers_map).ok();
        let body = response.text().ok();

        if let Some((meta, true)) = mock_state.take()
            && let Some(mock) = &self.mocks
        {
            let recorded = HttpMockResponse::new(status, headers_map.clone(), body.clone());
            mock.http_record(&meta, &recorded);
        }

        Ok(Ok(LegacyHttpResponse {
            status,
            headers_json,
            body,
        }))
    }
}

impl PackRuntime {
    #[allow(clippy::too_many_arguments)]
    pub async fn load(
        path: impl AsRef<Path>,
        config: Arc<HostConfig>,
        mocks: Option<Arc<MockLayer>>,
        archive_source: Option<&Path>,
        session_store: Option<DynSessionStore>,
        state_store: Option<DynStateStore>,
        wasi_policy: Arc<RunnerWasiPolicy>,
        verify_archive: bool,
    ) -> Result<Self> {
        let path = path.as_ref();
        let is_component = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("wasm"))
            .unwrap_or(false);
        if verify_archive && is_component {
            if let Some(archive) = archive_source {
                verify::verify_pack(archive).await?;
                tracing::info!(
                    pack_path = %archive.display(),
                    "pack verification complete (archive)"
                );
            }
        } else if verify_archive {
            verify::verify_pack(path).await?;
            tracing::info!(pack_path = %path.display(), "pack verification complete");
        }
        let engine = Engine::default();
        let wasm_bytes = fs::read(path).await?;
        let mut metadata =
            PackMetadata::from_wasm(&wasm_bytes).unwrap_or_else(|| PackMetadata::fallback(path));
        let mut archive = None;
        let component = match Component::from_file(&engine, path) {
            Ok(component) => Some(component),
            Err(err) => {
                if let Some(archive_path) = archive_source {
                    tracing::warn!(
                        error = %err,
                        pack = %archive_path.display(),
                        "component load failed, using manifest archive"
                    );
                    let archive_data = ArchiveFlows::from_archive(archive_path)?;
                    metadata = archive_data.metadata.clone();
                    archive = Some(archive_data);
                    None
                } else {
                    return Err(err);
                }
            }
        };
        Ok(Self {
            path: path.to_path_buf(),
            config,
            engine,
            component,
            metadata,
            mocks,
            archive,
            session_store,
            state_store,
            wasi_policy,
        })
    }

    pub async fn list_flows(&self) -> Result<Vec<FlowDescriptor>> {
        if let Some(archive) = &self.archive {
            return Ok(archive.descriptors.clone());
        }
        tracing::trace!(
            tenant = %self.config.tenant,
            pack_path = %self.path.display(),
            "listing flows from pack"
        );
        let component = self
            .component
            .as_ref()
            .ok_or_else(|| anyhow!("pack component unavailable"))?;
        let host_state = HostState::new(
            Arc::clone(&self.config),
            self.mocks.clone(),
            self.session_store.clone(),
            self.state_store.clone(),
        )?;
        let mut store = Store::new(
            &self.engine,
            ComponentState::new(host_state, Arc::clone(&self.wasi_policy))?,
        );
        let mut linker = Linker::new(&self.engine);
        imports::register_all(&mut linker)?;
        let bindings = pack_export_v0_2::PackExports::instantiate(&mut store, component, &linker)?;
        let exports = bindings.greentic_pack_export_exports();
        let flows_raw = match exports.call_list_flows(&mut store)? {
            Ok(flows) => flows,
            Err(err) => {
                bail!("pack list_flows failed: {err:?}");
            }
        };
        let flows = flows_raw
            .into_iter()
            .map(|flow: FlowInfo| {
                let profile = flow.profile.clone();
                let version = flow.version.clone();
                FlowDescriptor {
                    id: flow.id,
                    flow_type: flow.flow_type,
                    profile,
                    version,
                    description: Some(format!("{}@{}", flow.profile, flow.version)),
                }
            })
            .collect();
        Ok(flows)
    }

    #[allow(dead_code)]
    pub async fn run_flow(
        &self,
        _flow_id: &str,
        _input: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // TODO: dispatch flow execution via Wasmtime
        Ok(serde_json::json!({}))
    }

    pub fn load_flow_ir(&self, flow_id: &str) -> Result<greentic_flow::ir::FlowIR> {
        if let Some(archive) = &self.archive {
            return archive
                .flows
                .get(flow_id)
                .cloned()
                .ok_or_else(|| anyhow!("flow '{flow_id}' not found in archive"));
        }
        let component = self
            .component
            .as_ref()
            .ok_or_else(|| anyhow!("pack component unavailable"))?;
        let host_state = HostState::new(
            Arc::clone(&self.config),
            self.mocks.clone(),
            self.session_store.clone(),
            self.state_store.clone(),
        )?;
        let mut store = Store::new(
            &self.engine,
            ComponentState::new(host_state, Arc::clone(&self.wasi_policy))?,
        );
        let mut linker = Linker::new(&self.engine);
        imports::register_all(&mut linker)?;
        let bindings = pack_export_v0_2::PackExports::instantiate(&mut store, component, &linker)?;
        let exports = bindings.greentic_pack_export_exports();
        let flow_name = flow_id.to_string();
        let metadata = match exports.call_flow_metadata(&mut store, &flow_name)? {
            Ok(doc) => doc,
            Err(err) => bail!("pack flow_metadata({flow_id}) failed: {err:?}"),
        };
        let flow_doc: greentic_flow::model::FlowDoc = serde_json::from_str(&metadata)
            .or_else(|_| serde_yaml::from_str(&metadata))
            .with_context(|| format!("failed to parse flow metadata for {flow_id}"))?;
        let ir = greentic_flow::to_ir(flow_doc)?;
        Ok(ir)
    }

    pub fn metadata(&self) -> &PackMetadata {
        &self.metadata
    }
}

#[cfg(feature = "mcp")]
fn map_legacy_tenant_ctx(ctx: LegacyTenantCtx, default_env: &str) -> TypesTenantCtx {
    let env = ctx
        .deployment
        .runtime
        .unwrap_or_else(|| default_env.to_string());

    let env_id = EnvId::from_str(env.as_str()).expect("invalid env id");
    let tenant_id = TenantId::from_str(ctx.tenant.as_str()).expect("invalid tenant id");
    let mut tenant_ctx = TypesTenantCtx::new(env_id, tenant_id);
    tenant_ctx = tenant_ctx.with_team(
        ctx.team
            .as_ref()
            .and_then(|team| TeamId::from_str(team.as_str()).ok()),
    );
    tenant_ctx = tenant_ctx.with_user(
        ctx.user
            .as_ref()
            .and_then(|user| UserId::from_str(user.as_str()).ok()),
    );
    tenant_ctx.trace_id = ctx.trace_id;
    tenant_ctx
}

fn map_legacy_error(err: LegacyIfaceError) -> types::IfaceError {
    match err {
        LegacyIfaceError::InvalidArg => types::IfaceError::InvalidArg,
        LegacyIfaceError::NotFound => types::IfaceError::NotFound,
        LegacyIfaceError::Denied => types::IfaceError::Denied,
        LegacyIfaceError::Unavailable => types::IfaceError::Unavailable,
        LegacyIfaceError::Internal => types::IfaceError::Internal,
    }
}

struct ArchiveFlows {
    descriptors: Vec<FlowDescriptor>,
    flows: HashMap<String, FlowIR>,
    metadata: PackMetadata,
}

impl ArchiveFlows {
    fn from_archive(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open archive {}", path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("{} is not a valid gtpack", path.display()))?;
        let manifest_bytes = read_entry(&mut archive, "manifest.cbor")?;
        let manifest: greentic_pack::builder::PackManifest =
            serde_cbor::from_slice(&manifest_bytes).context("manifest.cbor is invalid")?;

        let mut flows = HashMap::new();
        let mut descriptors = Vec::new();
        for flow in &manifest.flows {
            let ir = build_stub_flow_ir(&flow.id, &flow.kind);
            flows.insert(flow.id.clone(), ir);
            descriptors.push(FlowDescriptor {
                id: flow.id.clone(),
                flow_type: flow.kind.clone(),
                profile: manifest.meta.name.clone(),
                version: manifest.meta.version.to_string(),
                description: Some(flow.kind.clone()),
            });
        }

        Ok(Self {
            metadata: PackMetadata::from_manifest(&manifest),
            descriptors,
            flows,
        })
    }
}

fn read_entry(archive: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let mut file = archive
        .by_name(name)
        .with_context(|| format!("entry {name} missing from archive"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn build_stub_flow_ir(flow_id: &str, flow_type: &str) -> FlowIR {
    let mut nodes = IndexMap::new();
    nodes.insert(
        "complete".into(),
        NodeIR {
            component: "qa.process".into(),
            payload_expr: json!({
                "status": "done",
                "flow_id": flow_id,
            }),
            routes: vec![RouteIR {
                to: None,
                out: true,
            }],
        },
    );
    FlowIR {
        id: flow_id.to_string(),
        flow_type: flow_type.to_string(),
        start: Some("complete".into()),
        parameters: Value::Object(Default::default()),
        nodes,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PackMetadata {
    pub pack_id: String,
    pub version: String,
    #[serde(default)]
    pub entry_flows: Vec<String>,
}

impl PackMetadata {
    fn from_wasm(bytes: &[u8]) -> Option<Self> {
        let parser = Parser::new(0);
        for payload in parser.parse_all(bytes) {
            let payload = payload.ok()?;
            match payload {
                Payload::CustomSection(section) => {
                    if section.name() == "greentic.manifest"
                        && let Ok(meta) = Self::from_bytes(section.data())
                    {
                        return Some(meta);
                    }
                }
                Payload::DataSection(reader) => {
                    for segment in reader.into_iter().flatten() {
                        if let Ok(meta) = Self::from_bytes(segment.data) {
                            return Some(meta);
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, serde_cbor::Error> {
        #[derive(Deserialize)]
        struct RawManifest {
            pack_id: String,
            version: String,
            #[serde(default)]
            entry_flows: Vec<String>,
            #[serde(default)]
            flows: Vec<RawFlow>,
        }

        #[derive(Deserialize)]
        struct RawFlow {
            id: String,
        }

        let manifest: RawManifest = serde_cbor::from_slice(bytes)?;
        let mut entry_flows = if manifest.entry_flows.is_empty() {
            manifest.flows.iter().map(|f| f.id.clone()).collect()
        } else {
            manifest.entry_flows.clone()
        };
        entry_flows.retain(|id| !id.is_empty());
        Ok(Self {
            pack_id: manifest.pack_id,
            version: manifest.version,
            entry_flows,
        })
    }

    pub fn fallback(path: &Path) -> Self {
        let pack_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown-pack".to_string());
        Self {
            pack_id,
            version: "0.0.0".to_string(),
            entry_flows: Vec::new(),
        }
    }

    pub fn from_manifest(manifest: &greentic_pack::builder::PackManifest) -> Self {
        let entry_flows = if manifest.meta.entry_flows.is_empty() {
            manifest
                .flows
                .iter()
                .map(|flow| flow.id.clone())
                .collect::<Vec<_>>()
        } else {
            manifest.meta.entry_flows.clone()
        };
        Self {
            pack_id: manifest.meta.pack_id.clone(),
            version: manifest.meta.version.to_string(),
            entry_flows,
        }
    }
}

impl From<&HttpMockResponse> for LegacyHttpResponse {
    fn from(value: &HttpMockResponse) -> Self {
        let headers_json = serde_json::to_string(&value.headers).ok();
        Self {
            status: value.status,
            headers_json,
            body: value.body.clone(),
        }
    }
}
