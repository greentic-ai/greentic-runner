use crate::config::{
    HostConfig, McpConfig, McpRetryConfig, RateLimits, SecretsPolicy, WebhookPolicy,
};
use crate::pack::{PackMetadata, PackRuntime};
use crate::runner::engine::{ExecutionObserver, FlowContext, FlowEngine, NodeEvent};
pub use crate::runner::mocks::{
    HttpMock, HttpMockMode, KvMock, MocksConfig, SecretsMock, TelemetryMock, TimeMock, ToolsMock,
};
use crate::runner::mocks::{MockEventSink, MockLayer};
use anyhow::{Context, Result, anyhow};
use greentic_pack::reader::{PackLoad, open_pack};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value, json};
use serde_yaml_bw::{Mapping as YamlMapping, Value as YamlValue};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::runtime::Runtime;
use tracing::{info, warn};
use uuid::Uuid;
use zip::ZipArchive;

const PROVIDER_ID_DEV: &str = "greentic-dev";

/// Hook invoked for each transcript event.
pub type TranscriptHook = Arc<dyn Fn(&Value) + Send + Sync>;

/// Configuration for emitting OTLP telemetry.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct OtlpHook {
    pub endpoint: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub sample_all: bool,
}

/// Execution profile.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Profile {
    Dev(DevProfile),
}

impl Default for Profile {
    fn default() -> Self {
        Self::Dev(DevProfile::default())
    }
}

/// Tunables for the developer profile.
#[derive(Clone, Debug)]
pub struct DevProfile {
    pub tenant_id: String,
    pub team_id: String,
    pub user_id: String,
    pub max_node_wall_time_ms: u64,
    pub max_run_wall_time_ms: u64,
}

impl Default for DevProfile {
    fn default() -> Self {
        Self {
            tenant_id: "local-dev".to_string(),
            team_id: "default".to_string(),
            user_id: "developer".to_string(),
            max_node_wall_time_ms: 30_000,
            max_run_wall_time_ms: 600_000,
        }
    }
}

/// Tenant context overrides supplied by callers.
#[derive(Clone, Debug, Default)]
pub struct TenantContext {
    pub tenant_id: Option<String>,
    pub team_id: Option<String>,
    pub user_id: Option<String>,
    pub session_id: Option<String>,
}

impl TenantContext {
    pub fn default_local() -> Self {
        Self {
            tenant_id: Some("local-dev".into()),
            team_id: Some("default".into()),
            user_id: Some("developer".into()),
            session_id: None,
        }
    }
}

/// User supplied options for a run.
#[derive(Clone)]
pub struct RunOptions {
    pub profile: Profile,
    pub entry_flow: Option<String>,
    pub input: Value,
    pub ctx: TenantContext,
    pub transcript: Option<TranscriptHook>,
    pub otlp: Option<OtlpHook>,
    pub mocks: MocksConfig,
    pub artifacts_dir: Option<PathBuf>,
    pub signing: SigningPolicy,
}

impl Default for RunOptions {
    fn default() -> Self {
        desktop_defaults()
    }
}

impl fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunOptions")
            .field("profile", &self.profile)
            .field("entry_flow", &self.entry_flow)
            .field("input", &self.input)
            .field("ctx", &self.ctx)
            .field("transcript", &self.transcript.is_some())
            .field("otlp", &self.otlp)
            .field("mocks", &self.mocks)
            .field("artifacts_dir", &self.artifacts_dir)
            .field("signing", &self.signing)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum SigningPolicy {
    Strict,
    DevOk,
}

/// Convenience builder that retains a baseline `RunOptions`.
#[derive(Clone, Debug)]
pub struct Runner {
    base: RunOptions,
}

impl Runner {
    pub fn new() -> Self {
        Self {
            base: desktop_defaults(),
        }
    }

    pub fn profile(mut self, profile: Profile) -> Self {
        self.base.profile = profile;
        self
    }

    pub fn with_mocks(mut self, mocks: MocksConfig) -> Self {
        self.base.mocks = mocks;
        self
    }

    pub fn configure(mut self, f: impl FnOnce(&mut RunOptions)) -> Self {
        f(&mut self.base);
        self
    }

    pub fn run_pack<P: AsRef<Path>>(&self, pack_path: P) -> Result<RunResult> {
        run_pack_with_options(pack_path, self.base.clone())
    }

    pub fn run_pack_with<P: AsRef<Path>>(
        &self,
        pack_path: P,
        f: impl FnOnce(&mut RunOptions),
    ) -> Result<RunResult> {
        let mut opts = self.base.clone();
        f(&mut opts);
        run_pack_with_options(pack_path, opts)
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

/// Execute a pack with the provided options.
pub fn run_pack_with_options<P: AsRef<Path>>(pack_path: P, opts: RunOptions) -> Result<RunResult> {
    let runtime = Runtime::new().context("failed to create tokio runtime")?;
    runtime.block_on(run_pack_async(pack_path.as_ref(), opts))
}

/// Reasonable defaults for local desktop invocations.
pub fn desktop_defaults() -> RunOptions {
    let otlp = std::env::var("OTLP_ENDPOINT")
        .ok()
        .map(|endpoint| OtlpHook {
            endpoint,
            headers: Vec::new(),
            sample_all: true,
        });
    RunOptions {
        profile: Profile::Dev(DevProfile::default()),
        entry_flow: None,
        input: json!({}),
        ctx: TenantContext::default_local(),
        transcript: None,
        otlp,
        mocks: MocksConfig::default(),
        artifacts_dir: None,
        signing: SigningPolicy::DevOk,
    }
}

async fn run_pack_async(pack_path: &Path, opts: RunOptions) -> Result<RunResult> {
    let resolved_profile = resolve_profile(&opts.profile, &opts.ctx);
    if let Some(otlp) = &opts.otlp {
        apply_otlp_hook(otlp);
    }

    let directories = prepare_run_dirs(opts.artifacts_dir.clone())?;
    info!(run_dir = %directories.root.display(), "prepared desktop run directory");

    let mock_layer = Arc::new(MockLayer::new(opts.mocks.clone(), &directories.root)?);

    let recorder = Arc::new(RunRecorder::new(
        directories.clone(),
        &resolved_profile,
        None,
        PackMetadata::fallback(pack_path),
        opts.transcript.clone(),
    )?);

    let mock_sink: Arc<dyn MockEventSink> = recorder.clone();
    mock_layer.register_sink(mock_sink);

    let mut pack_load: Option<PackLoad> = None;
    match open_pack(pack_path, to_reader_policy(opts.signing)) {
        Ok(load) => {
            recorder.update_pack_metadata(PackMetadata::from_manifest(&load.manifest));
            pack_load = Some(load);
        }
        Err(err) => {
            recorder.record_verify_event("error", &err.message)?;
            if opts.signing == SigningPolicy::DevOk && is_signature_error(&err.message) {
                warn!(error = %err.message, "continuing despite signature error (dev policy)");
            } else {
                return Err(anyhow!("pack verification failed: {}", err.message));
            }
        }
    }

    let host_config = Arc::new(build_host_config(&resolved_profile, &directories));
    let component_artifact =
        resolve_component_artifact(pack_path, pack_load.as_ref(), &directories)?;
    let archive_source = if pack_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("gtpack"))
        .unwrap_or(false)
    {
        Some(pack_path)
    } else {
        None
    };

    let pack = Arc::new(
        PackRuntime::load(
            &component_artifact,
            Arc::clone(&host_config),
            Some(Arc::clone(&mock_layer)),
            archive_source,
        )
        .await
        .with_context(|| format!("failed to load pack {}", component_artifact.display()))?,
    );
    recorder.update_pack_metadata(pack.metadata().clone());

    let flows = pack
        .list_flows()
        .await
        .context("failed to enumerate flows")?;
    let entry_flow_id = resolve_entry_flow(opts.entry_flow.clone(), pack.metadata(), &flows)?;
    recorder.set_flow_id(&entry_flow_id);

    let engine = FlowEngine::new(Arc::clone(&pack), Arc::clone(&host_config))
        .await
        .context("failed to prime flow engine")?;

    let started_at = OffsetDateTime::now_utc();
    let tenant_str = host_config.tenant.clone();
    let session_id_owned = resolved_profile.session_id.clone();
    let provider_id_owned = resolved_profile.provider_id.clone();
    let recorder_ref: &RunRecorder = &recorder;
    let mock_ref: &MockLayer = &mock_layer;
    let ctx = FlowContext {
        tenant: &tenant_str,
        flow_id: &entry_flow_id,
        node_id: None,
        tool: None,
        action: Some("run_pack"),
        session_id: Some(session_id_owned.as_str()),
        provider_id: Some(provider_id_owned.as_str()),
        retry_config: host_config.mcp_retry_config().into(),
        observer: Some(recorder_ref),
        mocks: Some(mock_ref),
    };

    let execution = engine.execute(ctx, opts.input.clone()).await;
    let finished_at = OffsetDateTime::now_utc();

    let status = match execution {
        Ok(_) => RunCompletion::Ok,
        Err(err) => RunCompletion::Err(err),
    };

    let result = recorder.finalise(status, started_at, finished_at)?;

    let run_json_path = directories.root.join("run.json");
    fs::write(&run_json_path, serde_json::to_vec_pretty(&result)?)
        .with_context(|| format!("failed to write run summary {}", run_json_path.display()))?;

    Ok(result)
}

fn apply_otlp_hook(hook: &OtlpHook) {
    info!(
        endpoint = %hook.endpoint,
        sample_all = hook.sample_all,
        headers = %hook.headers.len(),
        "OTLP hook requested (set OTEL_* env vars before invoking run_pack)"
    );
}

fn prepare_run_dirs(root_override: Option<PathBuf>) -> Result<RunDirectories> {
    let root = if let Some(dir) = root_override {
        dir
    } else {
        let timestamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
            .replace(':', "-");
        let short_id = Uuid::new_v4()
            .to_string()
            .chars()
            .take(6)
            .collect::<String>();
        PathBuf::from(".greentic")
            .join("runs")
            .join(format!("{}_{}", timestamp, short_id))
    };

    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    let logs = root.join("logs");
    let resolved = root.join("resolved_config");
    fs::create_dir_all(&logs).with_context(|| format!("failed to create {}", logs.display()))?;
    fs::create_dir_all(&resolved)
        .with_context(|| format!("failed to create {}", resolved.display()))?;

    Ok(RunDirectories {
        root,
        logs,
        resolved,
    })
}

fn resolve_entry_flow(
    override_id: Option<String>,
    metadata: &PackMetadata,
    flows: &[crate::pack::FlowDescriptor],
) -> Result<String> {
    if let Some(flow) = override_id {
        return Ok(flow);
    }
    if let Some(first) = metadata.entry_flows.first() {
        return Ok(first.clone());
    }
    flows
        .first()
        .map(|f| f.id.clone())
        .ok_or_else(|| anyhow!("pack does not declare any flows"))
}

fn is_signature_error(message: &str) -> bool {
    message.to_ascii_lowercase().contains("signature")
}

fn to_reader_policy(policy: SigningPolicy) -> greentic_pack::reader::SigningPolicy {
    match policy {
        SigningPolicy::Strict => greentic_pack::reader::SigningPolicy::Strict,
        SigningPolicy::DevOk => greentic_pack::reader::SigningPolicy::DevOk,
    }
}

fn resolve_component_artifact(
    pack_path: &Path,
    load: Option<&PackLoad>,
    dirs: &RunDirectories,
) -> Result<PathBuf> {
    if pack_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("wasm"))
        .unwrap_or(false)
    {
        return Ok(pack_path.to_path_buf());
    }

    let entry = find_component_entry(pack_path, load)?;
    let file =
        File::open(pack_path).with_context(|| format!("failed to open {}", pack_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("{} is not a valid gtpack", pack_path.display()))?;
    let mut component = archive
        .by_name(&entry)
        .with_context(|| format!("component {} missing from pack", entry))?;
    let out_path = dirs.root.join("component.wasm");
    let mut out_file = File::create(&out_path)
        .with_context(|| format!("failed to create {}", out_path.display()))?;
    std::io::copy(&mut component, &mut out_file)
        .with_context(|| format!("failed to extract component {}", entry))?;
    Ok(out_path)
}

fn find_component_entry(pack_path: &Path, load: Option<&PackLoad>) -> Result<String> {
    if let Some(load) = load
        && let Some(component) = load.manifest.components.first()
    {
        return Ok(component.file_wasm.clone());
    }

    let file =
        File::open(pack_path).with_context(|| format!("failed to open {}", pack_path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("{} is not a valid gtpack", pack_path.display()))?;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read entry #{index}"))?;
        if entry.name().ends_with(".wasm") {
            return Ok(entry.name().to_string());
        }
    }
    Err(anyhow!("pack does not contain a wasm component"))
}

fn resolve_profile(profile: &Profile, ctx: &TenantContext) -> ResolvedProfile {
    match profile {
        Profile::Dev(dev) => ResolvedProfile {
            tenant_id: ctx
                .tenant_id
                .clone()
                .unwrap_or_else(|| dev.tenant_id.clone()),
            team_id: ctx.team_id.clone().unwrap_or_else(|| dev.team_id.clone()),
            user_id: ctx.user_id.clone().unwrap_or_else(|| dev.user_id.clone()),
            session_id: ctx
                .session_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            provider_id: PROVIDER_ID_DEV.to_string(),
            max_node_wall_time_ms: dev.max_node_wall_time_ms,
            max_run_wall_time_ms: dev.max_run_wall_time_ms,
        },
    }
}

fn build_host_config(profile: &ResolvedProfile, dirs: &RunDirectories) -> HostConfig {
    let mut store = YamlMapping::new();
    store.insert(YamlValue::from("kind"), YamlValue::from("local-dir"));
    store.insert(
        YamlValue::from("path"),
        YamlValue::from("./.greentic/tools"),
    );
    let mut runtime = YamlMapping::new();
    runtime.insert(YamlValue::from("max_memory_mb"), YamlValue::from(256u64));
    runtime.insert(
        YamlValue::from("timeout_ms"),
        YamlValue::from(profile.max_node_wall_time_ms),
    );
    runtime.insert(YamlValue::from("fuel"), YamlValue::from(50_000_000u64));
    let mut security = YamlMapping::new();
    security.insert(YamlValue::from("require_signature"), YamlValue::from(false));
    HostConfig {
        tenant: profile.tenant_id.clone(),
        bindings_path: dirs.resolved.join("dev.bindings.yaml"),
        flow_type_bindings: HashMap::new(),
        mcp: McpConfig {
            store: YamlValue::Mapping(store),
            security: YamlValue::Mapping(security),
            runtime: YamlValue::Mapping(runtime),
            http_enabled: Some(false),
            retry: Some(McpRetryConfig::default()),
        },
        rate_limits: RateLimits::default(),
        http_enabled: false,
        secrets_policy: SecretsPolicy::allow_all(),
        webhook_policy: WebhookPolicy::default(),
        timers: Vec::new(),
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
struct ResolvedProfile {
    tenant_id: String,
    team_id: String,
    user_id: String,
    session_id: String,
    provider_id: String,
    max_node_wall_time_ms: u64,
    max_run_wall_time_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunResult {
    pub session_id: String,
    pub pack_id: String,
    pub pack_version: String,
    pub flow_id: String,
    pub started_at_utc: String,
    pub finished_at_utc: String,
    pub status: RunStatus,
    pub node_summaries: Vec<NodeSummary>,
    pub failures: BTreeMap<String, NodeFailure>,
    pub artifacts_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    Success,
    PartialFailure,
    Failure,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSummary {
    pub node_id: String,
    pub component: String,
    pub status: NodeStatus,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum NodeStatus {
    Ok,
    Skipped,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeFailure {
    pub code: String,
    pub message: String,
    pub details: Value,
    pub transcript_offsets: (u64, u64),
    pub log_paths: Vec<PathBuf>,
}

#[derive(Clone)]
struct RunDirectories {
    root: PathBuf,
    logs: PathBuf,
    resolved: PathBuf,
}

enum RunCompletion {
    Ok,
    Err(anyhow::Error),
}

struct RunRecorder {
    directories: RunDirectories,
    profile: ResolvedProfile,
    flow_id: Mutex<String>,
    pack_meta: Mutex<PackMetadata>,
    transcript: Mutex<TranscriptWriter>,
    state: Mutex<RunRecorderState>,
}

impl RunRecorder {
    fn new(
        dirs: RunDirectories,
        profile: &ResolvedProfile,
        flow_id: Option<String>,
        pack_meta: PackMetadata,
        hook: Option<TranscriptHook>,
    ) -> Result<Self> {
        let transcript_path = dirs.root.join("transcript.jsonl");
        let file = File::create(&transcript_path)
            .with_context(|| format!("failed to open {}", transcript_path.display()))?;
        Ok(Self {
            directories: dirs,
            profile: profile.clone(),
            flow_id: Mutex::new(flow_id.unwrap_or_else(|| "unknown".into())),
            pack_meta: Mutex::new(pack_meta),
            transcript: Mutex::new(TranscriptWriter::new(BufWriter::new(file), hook)),
            state: Mutex::new(RunRecorderState::default()),
        })
    }

    fn finalise(
        &self,
        completion: RunCompletion,
        started_at: OffsetDateTime,
        finished_at: OffsetDateTime,
    ) -> Result<RunResult> {
        let status = match &completion {
            RunCompletion::Ok => RunStatus::Success,
            RunCompletion::Err(_) => RunStatus::Failure,
        };

        if let RunCompletion::Err(err) = completion {
            warn!(error = %err, "pack execution failed");
        }

        let started = started_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| started_at.to_string());
        let finished = finished_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| finished_at.to_string());

        let state = self.state.lock();
        let mut summaries = Vec::new();
        let mut failures = BTreeMap::new();
        for node_id in &state.order {
            if let Some(record) = state.nodes.get(node_id) {
                let duration = record.duration_ms.unwrap_or(0);
                summaries.push(NodeSummary {
                    node_id: node_id.clone(),
                    component: record.component.clone(),
                    status: record.status.clone(),
                    duration_ms: duration,
                });
                if record.status == NodeStatus::Error {
                    let start_offset = record.transcript_start.unwrap_or(0);
                    let err_offset = record.transcript_error.unwrap_or(start_offset);
                    failures.insert(
                        node_id.clone(),
                        NodeFailure {
                            code: record
                                .failure_code
                                .clone()
                                .unwrap_or_else(|| "component-failed".into()),
                            message: record
                                .failure_message
                                .clone()
                                .unwrap_or_else(|| "node failed".into()),
                            details: record
                                .failure_details
                                .clone()
                                .unwrap_or_else(|| json!({ "node": node_id })),
                            transcript_offsets: (start_offset, err_offset),
                            log_paths: record.log_paths.clone(),
                        },
                    );
                }
            }
        }

        let mut final_status = status;
        if final_status == RunStatus::Success && !failures.is_empty() {
            final_status = RunStatus::PartialFailure;
        }

        let pack_meta = self.pack_meta.lock();
        let flow_id = self.flow_id.lock().clone();
        Ok(RunResult {
            session_id: self.profile.session_id.clone(),
            pack_id: pack_meta.pack_id.clone(),
            pack_version: pack_meta.version.clone(),
            flow_id,
            started_at_utc: started,
            finished_at_utc: finished,
            status: final_status,
            node_summaries: summaries,
            failures,
            artifacts_dir: self.directories.root.clone(),
        })
    }

    fn set_flow_id(&self, flow_id: &str) {
        *self.flow_id.lock() = flow_id.to_string();
    }

    fn update_pack_metadata(&self, meta: PackMetadata) {
        *self.pack_meta.lock() = meta;
    }

    fn current_flow_id(&self) -> String {
        self.flow_id.lock().clone()
    }

    fn record_verify_event(&self, status: &str, message: &str) -> Result<()> {
        let timestamp = OffsetDateTime::now_utc();
        let event = json!({
            "ts": timestamp
                .format(&Rfc3339)
                .unwrap_or_else(|_| timestamp.to_string()),
            "session_id": self.profile.session_id,
            "flow_id": self.current_flow_id(),
            "node_id": Value::Null,
            "component": "verify.pack",
            "phase": "verify",
            "status": status,
            "inputs": Value::Null,
            "outputs": Value::Null,
            "error": json!({ "message": message }),
            "metrics": Value::Null,
            "schema_id": Value::Null,
            "defaults_applied": Value::Null,
            "redactions": Value::Array(Vec::new()),
        });
        self.transcript.lock().write(&event).map(|_| ())
    }

    fn write_mock_event(&self, capability: &str, provider: &str, payload: Value) -> Result<()> {
        let timestamp = OffsetDateTime::now_utc();
        let event = json!({
            "ts": timestamp
                .format(&Rfc3339)
                .unwrap_or_else(|_| timestamp.to_string()),
            "session_id": self.profile.session_id,
            "flow_id": self.current_flow_id(),
            "node_id": Value::Null,
            "component": format!("mock::{capability}"),
            "phase": "mock",
            "status": "ok",
            "inputs": json!({ "capability": capability, "provider": provider }),
            "outputs": payload,
            "error": Value::Null,
            "metrics": Value::Null,
            "schema_id": Value::Null,
            "defaults_applied": Value::Null,
            "redactions": Value::Array(Vec::new()),
        });
        self.transcript.lock().write(&event).map(|_| ())
    }

    fn handle_node_start(&self, event: &NodeEvent<'_>) -> Result<()> {
        let timestamp = OffsetDateTime::now_utc();
        let (redacted_payload, redactions) = redact_value(event.payload, "$.inputs.payload");
        let inputs = json!({
            "payload": redacted_payload,
            "context": {
                "tenant_id": self.profile.tenant_id.as_str(),
                "team_id": self.profile.team_id.as_str(),
                "user_id": self.profile.user_id.as_str(),
            }
        });
        let flow_id = self.current_flow_id();
        let event_json = build_transcript_event(TranscriptEventArgs {
            profile: &self.profile,
            flow_id: &flow_id,
            node_id: event.node_id,
            component: &event.node.component,
            phase: "start",
            status: "ok",
            timestamp,
            inputs,
            outputs: Value::Null,
            error: Value::Null,
            redactions,
        });
        let (start_offset, _) = self.transcript.lock().write(&event_json)?;

        let mut state = self.state.lock();
        let node_key = event.node_id.to_string();
        if !state.order.iter().any(|id| id == &node_key) {
            state.order.push(node_key.clone());
        }
        let entry = state.nodes.entry(node_key).or_insert_with(|| {
            NodeExecutionRecord::new(event.node.component.clone(), &self.directories)
        });
        entry.start_instant = Some(Instant::now());
        entry.status = NodeStatus::Ok;
        entry.transcript_start = Some(start_offset);
        Ok(())
    }

    fn handle_node_end(&self, event: &NodeEvent<'_>, output: &Value) -> Result<()> {
        let timestamp = OffsetDateTime::now_utc();
        let (redacted_output, redactions) = redact_value(output, "$.outputs");
        let flow_id = self.current_flow_id();
        let event_json = build_transcript_event(TranscriptEventArgs {
            profile: &self.profile,
            flow_id: &flow_id,
            node_id: event.node_id,
            component: &event.node.component,
            phase: "end",
            status: "ok",
            timestamp,
            inputs: Value::Null,
            outputs: redacted_output,
            error: Value::Null,
            redactions,
        });
        self.transcript.lock().write(&event_json)?;

        let mut state = self.state.lock();
        if let Some(entry) = state.nodes.get_mut(event.node_id)
            && let Some(started) = entry.start_instant.take()
        {
            entry.duration_ms = Some(started.elapsed().as_millis() as u64);
        }
        Ok(())
    }

    fn handle_node_error(
        &self,
        event: &NodeEvent<'_>,
        error: &dyn std::error::Error,
    ) -> Result<()> {
        let timestamp = OffsetDateTime::now_utc();
        let error_message = error.to_string();
        let error_json = json!({
            "code": "component-failed",
            "message": error_message,
            "details": {
                "node": event.node_id,
            }
        });
        let flow_id = self.current_flow_id();
        let event_json = build_transcript_event(TranscriptEventArgs {
            profile: &self.profile,
            flow_id: &flow_id,
            node_id: event.node_id,
            component: &event.node.component,
            phase: "error",
            status: "error",
            timestamp,
            inputs: Value::Null,
            outputs: Value::Null,
            error: error_json.clone(),
            redactions: Vec::new(),
        });
        let (_, end_offset) = self.transcript.lock().write(&event_json)?;

        let mut state = self.state.lock();
        if let Some(entry) = state.nodes.get_mut(event.node_id) {
            entry.status = NodeStatus::Error;
            if let Some(started) = entry.start_instant.take() {
                entry.duration_ms = Some(started.elapsed().as_millis() as u64);
            }
            entry.transcript_error = Some(end_offset);
            entry.failure_code = Some("component-failed".to_string());
            entry.failure_message = Some(error_message.clone());
            entry.failure_details = Some(error_json);
            let log_path = self
                .directories
                .logs
                .join(format!("{}.stderr.log", sanitize_id(event.node_id)));
            if let Ok(mut file) = File::create(&log_path) {
                let _ = writeln!(file, "{}", error_message);
            }
            entry.log_paths.push(log_path);
        }
        Ok(())
    }
}

impl ExecutionObserver for RunRecorder {
    fn on_node_start(&self, event: &NodeEvent<'_>) {
        if let Err(err) = self.handle_node_start(event) {
            warn!(node = event.node_id, error = %err, "failed to record node start");
        }
    }

    fn on_node_end(&self, event: &NodeEvent<'_>, output: &Value) {
        if let Err(err) = self.handle_node_end(event, output) {
            warn!(node = event.node_id, error = %err, "failed to record node end");
        }
    }

    fn on_node_error(&self, event: &NodeEvent<'_>, error: &dyn std::error::Error) {
        if let Err(err) = self.handle_node_error(event, error) {
            warn!(node = event.node_id, error = %err, "failed to record node error");
        }
    }
}

impl MockEventSink for RunRecorder {
    fn on_mock_event(&self, capability: &str, provider: &str, payload: &Value) {
        if let Err(err) = self.write_mock_event(capability, provider, payload.clone()) {
            warn!(?capability, ?provider, error = %err, "failed to record mock event");
        }
    }
}

#[derive(Default)]
struct RunRecorderState {
    nodes: BTreeMap<String, NodeExecutionRecord>,
    order: Vec<String>,
}

#[derive(Clone)]
struct NodeExecutionRecord {
    component: String,
    status: NodeStatus,
    duration_ms: Option<u64>,
    transcript_start: Option<u64>,
    transcript_error: Option<u64>,
    log_paths: Vec<PathBuf>,
    failure_code: Option<String>,
    failure_message: Option<String>,
    failure_details: Option<Value>,
    start_instant: Option<Instant>,
}

impl NodeExecutionRecord {
    fn new(component: String, _dirs: &RunDirectories) -> Self {
        Self {
            component,
            status: NodeStatus::Ok,
            duration_ms: None,
            transcript_start: None,
            transcript_error: None,
            log_paths: Vec::new(),
            failure_code: None,
            failure_message: None,
            failure_details: None,
            start_instant: None,
        }
    }
}

struct TranscriptWriter {
    writer: BufWriter<File>,
    offset: u64,
    hook: Option<TranscriptHook>,
}

impl TranscriptWriter {
    fn new(writer: BufWriter<File>, hook: Option<TranscriptHook>) -> Self {
        Self {
            writer,
            offset: 0,
            hook,
        }
    }

    fn write(&mut self, value: &Value) -> Result<(u64, u64)> {
        let line = serde_json::to_vec(value)?;
        let start = self.offset;
        self.writer.write_all(&line)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        self.offset += line.len() as u64 + 1;
        if let Some(hook) = &self.hook {
            hook(value);
        }
        Ok((start, self.offset))
    }
}

struct TranscriptEventArgs<'a> {
    profile: &'a ResolvedProfile,
    flow_id: &'a str,
    node_id: &'a str,
    component: &'a str,
    phase: &'a str,
    status: &'a str,
    timestamp: OffsetDateTime,
    inputs: Value,
    outputs: Value,
    error: Value,
    redactions: Vec<String>,
}

fn build_transcript_event(args: TranscriptEventArgs<'_>) -> Value {
    let ts = args
        .timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| args.timestamp.to_string());
    json!({
        "ts": ts,
        "session_id": args.profile.session_id.as_str(),
        "flow_id": args.flow_id,
        "node_id": args.node_id,
        "component": args.component,
        "phase": args.phase,
        "status": args.status,
        "inputs": args.inputs,
        "outputs": args.outputs,
        "error": args.error,
        "metrics": {
            "duration_ms": null,
            "cpu_time_ms": null,
            "mem_peak_bytes": null,
        },
        "schema_id": Value::Null,
        "defaults_applied": Value::Array(Vec::new()),
        "redactions": args.redactions,
    })
}

fn redact_value(value: &Value, base: &str) -> (Value, Vec<String>) {
    let mut paths = Vec::new();
    let redacted = redact_recursive(value, base, &mut paths);
    (redacted, paths)
}

fn redact_recursive(value: &Value, path: &str, acc: &mut Vec<String>) -> Value {
    match value {
        Value::Object(map) => {
            let mut new_map = JsonMap::new();
            for (key, val) in map {
                let child_path = format!("{}.{}", path, key);
                if is_sensitive_key(key) {
                    acc.push(child_path);
                    new_map.insert(key.clone(), Value::String("__REDACTED__".into()));
                } else {
                    new_map.insert(key.clone(), redact_recursive(val, &child_path, acc));
                }
            }
            Value::Object(new_map)
        }
        Value::Array(items) => {
            let mut new_items = Vec::new();
            for (idx, item) in items.iter().enumerate() {
                let child_path = format!("{}[{}]", path, idx);
                new_items.push(redact_recursive(item, &child_path, acc));
            }
            Value::Array(new_items)
        }
        other => other.clone(),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    const MARKERS: [&str; 5] = ["secret", "token", "password", "authorization", "cookie"];
    MARKERS.iter().any(|marker| lower.contains(marker))
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}
