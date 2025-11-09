use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(feature = "mcp")]
use std::str::FromStr;
use std::sync::Arc;

use crate::runtime_wasmtime::{Component, Engine, Linker, Store, WasmResult};
use anyhow::{Context, Result, anyhow, bail};
use greentic_flow::ir::{FlowIR, NodeIR, RouteIR};
use greentic_interfaces::host_import_v0_2::greentic::host_import::imports::{
    HttpRequest, HttpResponse, IfaceError, TenantCtx,
};
use greentic_interfaces::pack_export_v0_2;
use greentic_interfaces::pack_export_v0_2::exports::greentic::pack_export::exports::FlowInfo;
#[cfg(feature = "mcp")]
use greentic_mcp::{ExecConfig, ExecError, ExecRequest};
#[cfg(feature = "mcp")]
use greentic_types::{EnvId, TeamId, TenantCtx as TypesTenantCtx, TenantId, UserId};
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
use crate::verify;

pub struct PackRuntime {
    path: PathBuf,
    config: Arc<HostConfig>,
    engine: Engine,
    component: Option<Component>,
    metadata: PackMetadata,
    mocks: Option<Arc<MockLayer>>,
    archive: Option<ArchiveFlows>,
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
    #[cfg(feature = "mcp")]
    default_env: String,
    mocks: Option<Arc<MockLayer>>,
}

impl HostState {
    pub fn new(config: Arc<HostConfig>, mocks: Option<Arc<MockLayer>>) -> Result<Self> {
        let http_client = BlockingClient::builder().build()?;
        #[cfg(feature = "mcp")]
        let exec_config = config.mcp_exec_config().ok();
        #[cfg(feature = "mcp")]
        let default_env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
        Ok(Self {
            config,
            http_client,
            #[cfg(feature = "mcp")]
            exec_config,
            #[cfg(feature = "mcp")]
            default_env,
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
        ctx: Option<TenantCtx>,
    ) -> WasmResult<Result<String, IfaceError>> {
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (tool, action, args_json, ctx);
            tracing::warn!("tool invoke requested but crate built without `mcp` feature");
            return Ok(Err(IfaceError::Unavailable));
        }

        #[cfg(feature = "mcp")]
        {
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
                        Ok(Err(IfaceError::Internal))
                    }
                };
            }

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
                    let http = HttpResponse::from(&response);
                    return Ok(Ok(http));
                }
                HttpDecision::Deny(reason) => {
                    tracing::warn!(url = %req.url, reason = %reason, "http fetch blocked by mocks");
                    return Ok(Err(IfaceError::Denied));
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
                return Ok(Err(IfaceError::Unavailable));
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

        Ok(Ok(HttpResponse {
            status,
            headers_json,
            body,
        }))
    }
}

impl PackRuntime {
    pub async fn load(
        path: impl AsRef<Path>,
        config: Arc<HostConfig>,
        mocks: Option<Arc<MockLayer>>,
        archive_source: Option<&Path>,
    ) -> Result<Self> {
        let path = path.as_ref();
        verify::verify_pack(path).await?;
        tracing::info!(pack_path = %path.display(), "pack verification complete");
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
        let mut store = Store::new(
            &self.engine,
            HostState::new(Arc::clone(&self.config), self.mocks.clone())?,
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
        let mut store = Store::new(
            &self.engine,
            HostState::new(Arc::clone(&self.config), self.mocks.clone())?,
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
fn map_tenant_ctx(ctx: TenantCtx, default_env: &str) -> TypesTenantCtx {
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

impl From<&HttpMockResponse> for HttpResponse {
    fn from(value: &HttpMockResponse) -> Self {
        let headers_json = serde_json::to_string(&value.headers).ok();
        Self {
            status: value.status,
            headers_json,
            body: value.body.clone(),
        }
    }
}
