//! Host state for WASM component execution.

use crate::component_api::{
    self, node::ExecCtx as ComponentExecCtx, node::InvokeResult, node::NodeError,
};
use crate::config::HostConfig;
use crate::oauth::{OAuthBrokerConfig, OAuthBrokerHost};
use crate::provider_core_only;
use crate::runner::mocks::{HttpDecision, HttpMockRequest, HttpMockResponse, MockLayer};
use crate::runtime_wasmtime::{Component, Linker, Store};
use crate::secrets::{DynSecretsManager, read_secret_blocking};
use crate::storage::{DynSessionStore, DynStateStore};
use anyhow::{Context, Result, anyhow, bail};
use futures::executor::block_on;
use greentic_interfaces_wasmtime::host_helpers::v1::http_client::{
    HttpClientError, Request as HttpRequest, RequestOptionsV1_1 as HttpRequestOptionsV1_1,
    Response as HttpResponse, TenantCtx as HttpTenantCtx,
};
use greentic_interfaces_wasmtime::host_helpers::v1::state_store::TenantCtx as StateTenantCtx;
use greentic_types::{EnvId, TeamId, TenantCtx as TypesTenantCtx, TenantId, UserId};
use reqwest::blocking::Client as BlockingClient;
use serde_json::Value;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use super::component_state::ComponentState;

/// Host state providing capabilities to WASM components.
pub struct HostState {
    #[allow(dead_code)]
    pub(crate) pack_id: String,
    pub(crate) config: Arc<HostConfig>,
    pub(crate) http_client: Arc<BlockingClient>,
    pub(crate) default_env: String,
    #[allow(dead_code)]
    pub(crate) session_store: Option<DynSessionStore>,
    pub(crate) state_store: Option<DynStateStore>,
    pub(crate) mocks: Option<Arc<MockLayer>>,
    pub(crate) secrets: DynSecretsManager,
    pub(crate) oauth_config: Option<OAuthBrokerConfig>,
    pub(crate) oauth_host: OAuthBrokerHost,
    pub(crate) exec_ctx: Option<ComponentExecCtx>,
    pub(crate) component_ref: Option<String>,
    pub(crate) provider_core_component: bool,
}

impl HostState {
    /// Create a new host state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pack_id: String,
        config: Arc<HostConfig>,
        http_client: Arc<BlockingClient>,
        mocks: Option<Arc<MockLayer>>,
        session_store: Option<DynSessionStore>,
        state_store: Option<DynStateStore>,
        secrets: DynSecretsManager,
        oauth_config: Option<OAuthBrokerConfig>,
        exec_ctx: Option<ComponentExecCtx>,
        component_ref: Option<String>,
        provider_core_component: bool,
    ) -> Result<Self> {
        let default_env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
        Ok(Self {
            pack_id,
            config,
            http_client,
            default_env,
            session_store,
            state_store,
            mocks,
            secrets,
            oauth_config,
            oauth_host: OAuthBrokerHost,
            exec_ctx,
            component_ref,
            provider_core_component,
        })
    }

    /// Build a TenantCtx for secrets lookups including team from exec context.
    pub(crate) fn secrets_tenant_ctx(&self) -> TypesTenantCtx {
        let mut ctx = self.config.tenant_ctx();
        if let Some(exec_ctx) = self.exec_ctx.as_ref()
            && let Some(team) = exec_ctx.tenant.team.as_ref()
            && let Ok(team_id) = TeamId::from_str(team)
        {
            ctx = ctx.with_team(Some(team_id));
        }
        ctx
    }

    /// Get a secret value.
    pub fn get_secret(&self, key: &str) -> Result<String> {
        if provider_core_only::is_enabled() {
            bail!(provider_core_only::blocked_message("secrets"))
        }
        if !self.config.secrets_policy.is_allowed(key) {
            bail!("secret {key} is not permitted by bindings policy");
        }
        if let Some(mock) = &self.mocks
            && let Some(value) = mock.secrets_lookup(key)
        {
            return Ok(value);
        }
        let ctx = self.secrets_tenant_ctx();
        let bytes = read_secret_blocking(&self.secrets, &ctx, &self.pack_id, key)
            .context("failed to read secret from manager")?;
        let value = String::from_utf8(bytes).context("secret value is not valid UTF-8")?;
        Ok(value)
    }

    /// Check if secret write is allowed in provider-core-only mode.
    pub(crate) fn allows_secret_write_in_provider_core_only(&self) -> bool {
        self.provider_core_component || self.component_ref.is_none()
    }

    /// Convert state tenant context to types tenant context.
    pub(crate) fn tenant_ctx_from_v1(&self, ctx: Option<StateTenantCtx>) -> Result<TypesTenantCtx> {
        let tenant_raw = ctx
            .as_ref()
            .map(|ctx| ctx.tenant.clone())
            .or_else(|| self.exec_ctx.as_ref().map(|ctx| ctx.tenant.tenant.clone()))
            .unwrap_or_else(|| self.config.tenant.clone());
        let env_raw = ctx
            .as_ref()
            .map(|ctx| ctx.env.clone())
            .unwrap_or_else(|| self.default_env.clone());
        let tenant_id = TenantId::from_str(&tenant_raw)
            .with_context(|| format!("invalid tenant id `{tenant_raw}`"))?;
        let env_id = EnvId::from_str(&env_raw)
            .unwrap_or_else(|_| EnvId::from_str("local").expect("default env must be valid"));
        let mut tenant_ctx = TypesTenantCtx::new(env_id, tenant_id);

        if let Some(exec_ctx) = self.exec_ctx.as_ref() {
            if let Some(team) = exec_ctx.tenant.team.as_ref() {
                let team_id =
                    TeamId::from_str(team).with_context(|| format!("invalid team id `{team}`"))?;
                tenant_ctx = tenant_ctx.with_team(Some(team_id));
            }
            if let Some(user) = exec_ctx.tenant.user.as_ref() {
                let user_id =
                    UserId::from_str(user).with_context(|| format!("invalid user id `{user}`"))?;
                tenant_ctx = tenant_ctx.with_user(Some(user_id));
            }
            tenant_ctx = tenant_ctx.with_flow(exec_ctx.flow_id.clone());
            if let Some(node) = exec_ctx.node_id.as_ref() {
                tenant_ctx = tenant_ctx.with_node(node.clone());
            }
            if let Some(session) = exec_ctx.tenant.correlation_id.as_ref() {
                tenant_ctx = tenant_ctx.with_session(session.clone());
            }
            tenant_ctx.trace_id = exec_ctx.tenant.trace_id.clone();
        }

        if let Some(ctx) = ctx {
            if let Some(team) = ctx.team.or(ctx.team_id) {
                let team_id =
                    TeamId::from_str(&team).with_context(|| format!("invalid team id `{team}`"))?;
                tenant_ctx = tenant_ctx.with_team(Some(team_id));
            }
            if let Some(user) = ctx.user.or(ctx.user_id) {
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

    /// Send an HTTP request.
    pub(crate) fn send_http_request(
        &mut self,
        req: HttpRequest,
        opts: Option<HttpRequestOptionsV1_1>,
        _ctx: Option<HttpTenantCtx>,
    ) -> Result<HttpResponse, HttpClientError> {
        if !self.config.http_enabled {
            return Err(HttpClientError {
                code: "denied".into(),
                message: "http client disabled by policy".into(),
            });
        }

        let mut mock_state = None;
        let raw_body = req.body.clone();
        if let Some(mock) = &self.mocks
            && let Ok(meta) = HttpMockRequest::new(&req.method, &req.url, raw_body.as_deref())
        {
            match mock.http_begin(&meta) {
                HttpDecision::Mock(response) => {
                    let headers = response
                        .headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();
                    return Ok(HttpResponse {
                        status: response.status,
                        headers,
                        body: response.body.clone().map(|b| b.into_bytes()),
                    });
                }
                HttpDecision::Deny(reason) => {
                    return Err(HttpClientError {
                        code: "denied".into(),
                        message: reason,
                    });
                }
                HttpDecision::Passthrough { record } => {
                    mock_state = Some((meta, record));
                }
            }
        }

        let method = req.method.parse().unwrap_or(reqwest::Method::GET);
        let mut builder = self.http_client.request(method, &req.url);
        for (key, value) in req.headers {
            if let Ok(header) = reqwest::header::HeaderName::from_bytes(key.as_bytes())
                && let Ok(header_value) = reqwest::header::HeaderValue::from_str(&value)
            {
                builder = builder.header(header, header_value);
            }
        }

        if let Some(body) = raw_body.clone() {
            builder = builder.body(body);
        }

        if let Some(opts) = opts {
            if let Some(timeout_ms) = opts.timeout_ms {
                builder = builder.timeout(Duration::from_millis(timeout_ms as u64));
            }
            if opts.allow_insecure == Some(true) {
                warn!(url = %req.url, "allow-insecure not supported; using default TLS validation");
            }
            if let Some(follow_redirects) = opts.follow_redirects
                && !follow_redirects
            {
                warn!(url = %req.url, "follow-redirects=false not supported; using default client behaviour");
            }
        }

        let response = match builder.send() {
            Ok(resp) => resp,
            Err(err) => {
                warn!(url = %req.url, error = %err, "http client request failed");
                return Err(HttpClientError {
                    code: "unavailable".into(),
                    message: err.to_string(),
                });
            }
        };

        let status = response.status().as_u16();
        let headers_vec = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let body_bytes = response.bytes().ok().map(|b| b.to_vec());

        if let Some((meta, true)) = mock_state.take()
            && let Some(mock) = &self.mocks
        {
            let recorded = HttpMockResponse::new(
                status,
                headers_vec.clone().into_iter().collect(),
                body_bytes
                    .as_ref()
                    .map(|b| String::from_utf8_lossy(b).into_owned()),
            );
            mock.http_record(&meta, &recorded);
        }

        Ok(HttpResponse {
            status,
            headers: headers_vec,
            body: body_bytes,
        })
    }

    /// Instantiate and invoke a component.
    pub(crate) fn instantiate_component_result(
        linker: &mut Linker<ComponentState>,
        store: &mut Store<ComponentState>,
        component: &Component,
        ctx: &ComponentExecCtx,
        operation: &str,
        input_json: &str,
    ) -> Result<InvokeResult> {
        let pre_instance = linker.instantiate_pre(component)?;
        match component_api::v0_5::ComponentPre::new(pre_instance) {
            Ok(pre) => {
                let result = block_on(async {
                    let bindings = pre.instantiate_async(&mut *store).await?;
                    let node = bindings.greentic_component_node();
                    let ctx_v05 = component_api::exec_ctx_v0_5(ctx);
                    let operation_owned = operation.to_string();
                    let input_owned = input_json.to_string();
                    node.call_invoke(&mut *store, &ctx_v05, &operation_owned, &input_owned)
                })?;
                Ok(component_api::invoke_result_from_v0_5(result))
            }
            Err(err) => {
                if is_missing_node_export(&err, "0.5.0") {
                    let pre_instance = linker.instantiate_pre(component)?;
                    match component_api::v0_4::ComponentPre::new(pre_instance) {
                        Ok(pre) => {
                            let result = block_on(async {
                                let bindings = pre.instantiate_async(&mut *store).await?;
                                let node = bindings.greentic_component_node();
                                let ctx_v04 = component_api::exec_ctx_v0_4(ctx);
                                let operation_owned = operation.to_string();
                                let input_owned = input_json.to_string();
                                node.call_invoke(
                                    &mut *store,
                                    &ctx_v04,
                                    &operation_owned,
                                    &input_owned,
                                )
                            })?;
                            Ok(component_api::invoke_result_from_v0_4(result))
                        }
                        Err(err_v04) => {
                            if is_missing_node_export(&err_v04, "0.4.0") {
                                Self::try_v06_runtime(linker, store, component, input_json)
                            } else {
                                Err(err_v04.into())
                            }
                        }
                    }
                } else {
                    Err(err.into())
                }
            }
        }
    }

    /// Fallback for v0.6 components using component-runtime::run.
    fn try_v06_runtime(
        linker: &mut Linker<ComponentState>,
        store: &mut Store<ComponentState>,
        component: &Component,
        input_json: &str,
    ) -> Result<InvokeResult> {
        let pre_instance = linker.instantiate_pre(component)?;
        let pre = component_api::v0_6_runtime::ComponentV0V6RuntimePre::new(pre_instance).map_err(
            |e| anyhow!("component exports neither node@0.5/0.4 nor component-runtime@0.6: {e}"),
        )?;

        let result = block_on(async {
            let bindings = pre.instantiate_async(&mut *store).await?;
            let runtime = bindings.greentic_component_component_runtime();

            let input_value: Value = serde_json::from_str(input_json).unwrap_or(Value::Null);
            let input_cbor =
                serde_cbor::to_vec(&input_value).context("encode input as CBOR for v0.6")?;
            let empty_state = serde_cbor::to_vec(&Value::Object(Default::default()))
                .context("encode empty state")?;

            let run_result = runtime
                .call_run(&mut *store, &input_cbor, &empty_state)
                .map_err(|e| anyhow!("v0.6 component-runtime::run call failed: {e}"))?;

            let output_value: Value = serde_cbor::from_slice(&run_result.output)
                .context("decode v0.6 run output CBOR")?;
            let output_json = serde_json::to_string(&output_value)
                .context("serialize v0.6 run output to JSON")?;

            Ok::<_, anyhow::Error>(output_json)
        })?;

        Ok(InvokeResult::Ok(result))
    }

    /// Convert invoke result to JSON value.
    pub(crate) fn convert_invoke_result(result: InvokeResult) -> Result<Value> {
        match result {
            InvokeResult::Ok(body) => {
                if body.is_empty() {
                    return Ok(Value::Null);
                }
                serde_json::from_str(&body).or_else(|_| Ok(Value::String(body)))
            }
            InvokeResult::Err(NodeError {
                code,
                message,
                retryable,
                backoff_ms,
                details,
            }) => {
                let mut obj = serde_json::Map::new();
                obj.insert("ok".into(), Value::Bool(false));
                let mut error = serde_json::Map::new();
                error.insert("code".into(), Value::String(code));
                error.insert("message".into(), Value::String(message));
                error.insert("retryable".into(), Value::Bool(retryable));
                if let Some(backoff) = backoff_ms {
                    error.insert("backoff_ms".into(), Value::Number(backoff.into()));
                }
                if let Some(details) = details {
                    error.insert(
                        "details".into(),
                        serde_json::from_str(&details).unwrap_or(Value::String(details)),
                    );
                }
                obj.insert("error".into(), Value::Object(error));
                Ok(Value::Object(obj))
            }
        }
    }
}

fn is_missing_node_export(err: &wasmtime::Error, version: &str) -> bool {
    let message = err.to_string();
    message.contains("no exported instance named")
        && message.contains(&format!("greentic:component/node@{version}"))
}
