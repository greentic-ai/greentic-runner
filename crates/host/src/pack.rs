use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::runtime_wasmtime::{Component, Engine, Linker, Store, WasmResult};
use anyhow::{bail, Context, Result};
use greentic_interfaces::host_import_v0_2::greentic::host_import::imports::{
    HttpRequest, HttpResponse, IfaceError, TenantCtx,
};
use greentic_interfaces::pack_export_v0_2;
use greentic_interfaces::pack_export_v0_2::exports::greentic::pack_export::exports::FlowInfo;
use greentic_mcp::{ExecConfig, ExecError, ExecRequest};
use greentic_types::{EnvId, TeamId, TenantCtx as TypesTenantCtx, TenantId, UserId};
use reqwest::blocking::Client as BlockingClient;
use serde::{Deserialize, Serialize};
use serde_json::{self, Value};
use serde_yaml_bw as serde_yaml;

use crate::imports;

use crate::config::HostConfig;
use crate::verify;

pub struct PackRuntime {
    path: PathBuf,
    config: Arc<HostConfig>,
    engine: Engine,
    component: Component,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowDescriptor {
    pub id: String,
    #[serde(rename = "type")]
    pub flow_type: String,
    #[serde(default)]
    pub description: Option<String>,
}

pub struct HostState {
    config: Arc<HostConfig>,
    http_client: BlockingClient,
    exec_config: Option<ExecConfig>,
    default_env: String,
}

impl HostState {
    pub fn new(config: Arc<HostConfig>) -> Result<Self> {
        let http_client = BlockingClient::builder().build()?;
        let exec_config = config.mcp_exec_config().ok();
        let default_env = env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
        Ok(Self {
            config,
            http_client,
            exec_config,
            default_env,
        })
    }

    pub fn get_secret(&self, key: &str) -> Result<String> {
        if !self.config.secrets_policy.is_allowed(key) {
            bail!("secret {key} is not permitted by bindings policy");
        }
        if let Ok(value) = std::env::var(key) {
            return Ok(value);
        }
        bail!("secret {key} not found in environment");
    }
}

impl greentic_interfaces::host_import_v0_2::HostImports for HostState {
    fn secrets_get(
        &mut self,
        key: String,
        _ctx: Option<TenantCtx>,
    ) -> WasmResult<Result<String, IfaceError>> {
        Ok(self.get_secret(&key).map_err(|err| {
            tracing::warn!(secret = %key, error = %err, "secret lookup denied");
            IfaceError::Denied
        }))
    }

    fn telemetry_emit(&mut self, span_json: String, _ctx: Option<TenantCtx>) -> WasmResult<()> {
        tracing::info!(span = %span_json, "telemetry emit from pack");
        Ok(())
    }

    fn tool_invoke(
        &mut self,
        tool: String,
        action: String,
        args_json: String,
        ctx: Option<TenantCtx>,
    ) -> WasmResult<Result<String, IfaceError>> {
        let exec_config = match &self.exec_config {
            Some(cfg) => cfg.clone(),
            None => {
                tracing::warn!(%tool, %action, "exec config unavailable for tool invoke");
                return Ok(Err(IfaceError::Unavailable));
            }
        };

        let args: Value = match serde_json::from_str(&args_json) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(error = %err, "invalid args for tool invoke");
                return Ok(Err(IfaceError::InvalidArg));
            }
        };

        let tenant = ctx.map(|ctx| map_tenant_ctx(ctx, &self.default_env));

        let request = ExecRequest {
            component: tool.clone(),
            action: action.clone(),
            args,
            tenant,
        };

        match greentic_mcp::exec(request, &exec_config) {
            Ok(value) => match serde_json::to_string(&value) {
                Ok(body) => Ok(Ok(body)),
                Err(err) => {
                    tracing::error!(error = %err, "failed to serialise tool result");
                    Ok(Err(IfaceError::Internal))
                }
            },
            Err(err) => {
                tracing::warn!(%tool, %action, error = %err, "tool invoke failed");
                let iface_err = match err {
                    ExecError::NotFound { .. } => IfaceError::NotFound,
                    ExecError::Tool { .. } => IfaceError::Denied,
                    _ => IfaceError::Unavailable,
                };
                Ok(Err(iface_err))
            }
        }
    }

    fn http_fetch(
        &mut self,
        req: HttpRequest,
        _ctx: Option<TenantCtx>,
    ) -> WasmResult<Result<HttpResponse, IfaceError>> {
        if !self.config.http_enabled {
            tracing::warn!(url = %req.url, "http fetch denied by policy");
            return Ok(Err(IfaceError::Denied));
        }

        let method = req.method.parse().unwrap_or(reqwest::Method::GET);

        let mut builder = self.http_client.request(method, &req.url);

        if let Some(headers_json) = req.headers_json.as_ref() {
            match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(headers_json) {
                Ok(map) => {
                    for (key, value) in map {
                        if let Some(val) = value.as_str() {
                            if let Ok(header) =
                                reqwest::header::HeaderName::from_bytes(key.as_bytes())
                            {
                                if let Ok(header_value) =
                                    reqwest::header::HeaderValue::from_str(val)
                                {
                                    builder = builder.header(header, header_value);
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to parse headers for http.fetch");
                }
            }
        }

        if let Some(body) = req.body {
            builder = builder.body(body);
        }

        let response = match builder.send() {
            Ok(resp) => resp,
            Err(err) => {
                tracing::error!(url = %req.url, error = %err, "http fetch failed");
                return Ok(Err(IfaceError::Unavailable));
            }
        };

        let status = response.status().as_u16();
        let headers_json = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    serde_json::Value::String(v.to_str().unwrap_or_default().to_string()),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let headers_json = serde_json::to_string(&headers_json).ok();
        let body = response.text().ok();

        Ok(Ok(HttpResponse {
            status,
            headers_json,
            body,
        }))
    }
}

impl PackRuntime {
    pub async fn load(path: impl AsRef<Path>, config: Arc<HostConfig>) -> Result<Self> {
        let path = path.as_ref();
        verify::verify_pack(path).await?;
        tracing::info!(pack_path = %path.display(), "pack verification complete");
        let engine = Engine::default();
        let component = Component::from_file(&engine, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            config,
            engine,
            component,
        })
    }

    pub async fn list_flows(&self) -> Result<Vec<FlowDescriptor>> {
        tracing::trace!(
            tenant = %self.config.tenant,
            pack_path = %self.path.display(),
            "listing flows from pack"
        );
        let mut store = Store::new(&self.engine, HostState::new(Arc::clone(&self.config))?);
        let mut linker = Linker::new(&self.engine);
        imports::register_all(&mut linker)?;
        let bindings =
            pack_export_v0_2::PackExports::instantiate(&mut store, &self.component, &linker)?;
        let exports = bindings.greentic_pack_export_exports();
        let flows_raw = match exports.call_list_flows(&mut store)? {
            Ok(flows) => flows,
            Err(err) => {
                bail!("pack list_flows failed: {:?}", err);
            }
        };
        let flows = flows_raw
            .into_iter()
            .map(|flow: FlowInfo| FlowDescriptor {
                id: flow.id,
                flow_type: flow.flow_type,
                description: Some(format!("{}@{}", flow.profile, flow.version)),
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
        let mut store = Store::new(&self.engine, HostState::new(Arc::clone(&self.config))?);
        let mut linker = Linker::new(&self.engine);
        imports::register_all(&mut linker)?;
        let bindings =
            pack_export_v0_2::PackExports::instantiate(&mut store, &self.component, &linker)?;
        let exports = bindings.greentic_pack_export_exports();
        let flow_name = flow_id.to_string();
        let metadata = match exports.call_flow_metadata(&mut store, &flow_name)? {
            Ok(doc) => doc,
            Err(err) => bail!("pack flow_metadata({flow_id}) failed: {:?}", err),
        };
        let flow_doc: greentic_flow::model::FlowDoc = serde_json::from_str(&metadata)
            .or_else(|_| serde_yaml::from_str(&metadata))
            .with_context(|| format!("failed to parse flow metadata for {flow_id}"))?;
        let ir = greentic_flow::to_ir(flow_doc)?;
        Ok(ir)
    }
}

fn map_tenant_ctx(ctx: TenantCtx, default_env: &str) -> TypesTenantCtx {
    let env = ctx
        .deployment
        .runtime
        .unwrap_or_else(|| default_env.to_string());

    TypesTenantCtx {
        env: EnvId::from(env.as_str()),
        tenant: TenantId::from(ctx.tenant.as_str()),
        team: ctx.team.map(|t| TeamId::from(t.as_str())),
        user: ctx.user.map(|u| UserId::from(u.as_str())),
        trace_id: ctx.trace_id,
        correlation_id: None,
        deadline: None,
        attempt: 0,
        idempotency_key: None,
    }
}
