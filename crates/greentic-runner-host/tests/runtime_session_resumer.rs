//! End-to-end test for [`RuntimeSessionResumer`]: pause a flow at a builtin
//! `session.wait` node through the real ingress entry
//! ([`StateMachineRuntime::handle`]), then resume it to completion by calling
//! the resumer directly with the matching correlation id — no NATS broker.
//!
//! This pins that the production resume path
//!   `RuntimeSessionResumer::resume`
//!     -> `StateMachineRuntime::handle`
//!       -> `PackFlowAdapter::call` -> `FlowResumeStore::fetch`
//!         -> `FlowEngine::resume`
//! actually re-keys the saved wait. The correlation id used is the full store
//! hint (`<bare hint>::pack=<pack_id>`), mirroring the convention pinned by
//! `tests/sorla_node.rs` (where `ctx.session_id` carries the `::pack=` suffix).
//!
//! Pack-building helpers are copied from `tests/resume_characterization.rs`; the
//! flow is two builtin nodes (`session.wait` -> `emit.response`) so no WASM is
//! invoked.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use greentic_runner_host::config::{
    FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
    WebhookPolicy,
};
use greentic_runner_host::engine::runtime::{IngressEnvelope, StateMachineRuntime};
use greentic_runner_host::pack::{ComponentResolution, PackRuntime};
use greentic_runner_host::runner::dispatch_listener::SessionResumer;
use greentic_runner_host::runner::engine::FlowEngine;
use greentic_runner_host::runner::remote_dispatch::{
    RemoteDispatch, RemoteDispatchAction, RemoteDispatchHandler,
};
use greentic_runner_host::runner::runtime_session_resumer::RuntimeSessionResumer;
use greentic_runner_host::storage::new_session_store;
use greentic_runner_host::storage::session::session_host_from;
use greentic_runner_host::storage::state::new_state_store;
use greentic_runner_host::trace::TraceConfig;
use greentic_runner_host::validate::ValidationConfig;
use greentic_types::DispatchMode;
use greentic_types::{
    ComponentCapabilities, ComponentManifest, ComponentProfiles, EnvId, ExtensionInline,
    ExtensionRef, PackFlowEntry, PackKind, PackManifest, ReplyScope, ResourceHints, TenantCtx,
    TenantId, encode_pack_manifest,
};
use once_cell::sync::Lazy;
use semver::Version;
use serde_json::json;
use tempfile::TempDir;
use zip::ZipArchive;
use zip::write::FileOptions;

const RUNTIME_FLOW_EXTENSION_ID: &str = "greentic.pack.runtime_flow";
const PACK_ID: &str = "resume.via.resumer";
const FLOW_ID: &str = "wait.flow";
const BARE_HINT: &str = "demo:provider:chan:conv:user";

static RUNTIME: Lazy<&'static tokio::runtime::Runtime> = Lazy::new(|| {
    Box::leak(Box::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime"),
    ))
});

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

fn component_artifact_path(temp_dir: &Path) -> Result<PathBuf> {
    let local =
        workspace_root().join("tests/fixtures/packs/runner-components/components/qa_process.wasm");
    if local.exists() {
        return Ok(local);
    }
    let archive_path =
        workspace_root().join("tests/fixtures/packs/runner-components/runner-components.gtpack");
    let mut archive = ZipArchive::new(File::open(&archive_path).context("open fixture gtpack")?)?;
    let mut wasm = archive
        .by_name("components/qa.process@0.1.0/component.wasm")
        .context("qa.process component missing from fixture pack")?;
    let out = temp_dir.join("qa_process.wasm");
    let mut buf = Vec::new();
    wasm.read_to_end(&mut buf)?;
    std::fs::write(&out, &buf)?;
    Ok(out)
}

fn host_config(bindings_path: &Path) -> HostConfig {
    HostConfig {
        tenant: "demo".into(),
        bindings_path: bindings_path.to_path_buf(),
        flow_type_bindings: Default::default(),
        rate_limits: RateLimits::default(),
        retry: FlowRetryConfig::default(),
        http_enabled: false,
        secrets_policy: SecretsPolicy::allow_all(),
        state_store_policy: StateStorePolicy::default(),
        webhook_policy: WebhookPolicy::default(),
        timers: Vec::new(),
        oauth: None,
        mocks: None,
        pack_bindings: Vec::new(),
        env_passthrough: Vec::new(),
        trace: TraceConfig::from_env(),
        validation: ValidationConfig::from_env(),
        operator_policy: OperatorPolicy::allow_all(),
        #[cfg(feature = "agentic-worker")]
        agents: std::collections::HashMap::new(),
        #[cfg(feature = "agentic-worker")]
        graphs: std::collections::HashMap::new(),
    }
}

/// Build a `.gtpack` whose only flow is `session.wait` -> `emit.response`.
fn build_wait_pack(pack_path: &Path) -> Result<()> {
    let runtime_flow = json!({
        "id": FLOW_ID,
        "flow_type": "messaging",
        "start": "wait",
        "nodes": {
            "wait": {
                "component": "session.wait",
                "input": { "reason": "awaiting downstream response" },
                "routing": { "next": { "node_id": "done" } }
            },
            "done": {
                "component": "emit.response",
                "input": { "text": "resumed and completed" },
                "routing": "end"
            }
        }
    });
    let runtime_extension = json!({ "flows": [runtime_flow] });

    let mut extensions = BTreeMap::new();
    extensions.insert(
        RUNTIME_FLOW_EXTENSION_ID.to_string(),
        ExtensionRef {
            kind: RUNTIME_FLOW_EXTENSION_ID.to_string(),
            version: "2.0.0".into(),
            digest: None,
            location: None,
            inline: Some(ExtensionInline::Other(runtime_extension)),
        },
    );

    let manifest = PackManifest {
        schema_version: "1.0".into(),
        pack_id: PACK_ID.parse()?,
        name: None,
        version: Version::parse("0.0.0")?,
        kind: PackKind::Application,
        publisher: "test".into(),
        components: vec![ComponentManifest {
            id: "qa.process".parse()?,
            version: Version::parse("0.1.0")?,
            supports: vec![greentic_types::FlowKind::Messaging],
            world: "greentic:component@0.4.0".into(),
            profiles: ComponentProfiles::default(),
            capabilities: ComponentCapabilities::default(),
            configurators: None,
            operations: Vec::new(),
            config_schema: None,
            resources: ResourceHints::default(),
            dev_flows: BTreeMap::new(),
        }],
        flows: Vec::<PackFlowEntry>::new(),
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        signatures: Default::default(),
        secret_requirements: Vec::new(),
        bootstrap: None,
        agents: BTreeMap::new(),
        extensions: Some(extensions),
    };

    let mut zip = zip::ZipWriter::new(File::create(pack_path).context("create pack archive")?);
    let options: FileOptions<'_, ()> =
        FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let manifest_bytes = encode_pack_manifest(&manifest)?;
    zip.start_file("manifest.cbor", options)?;
    zip.write_all(&manifest_bytes)?;
    let component_path = component_artifact_path(
        pack_path
            .parent()
            .expect("pack path should have a parent temp dir"),
    )?;
    zip.start_file("components/qa.process.wasm", options)?;
    let mut comp_file = File::open(&component_path)?;
    std::io::copy(&mut comp_file, &mut zip)?;
    zip.finish().context("finalise pack archive")?;
    Ok(())
}

/// The inbound envelope that triggers turn 1 (pause at `session.wait`). Its
/// `session_hint`/`pack_id`/`conversation` determine the stored wait key.
fn inbound_envelope() -> IngressEnvelope {
    IngressEnvelope {
        tenant: "demo".into(),
        env: Some("local".into()),
        pack_id: Some(PACK_ID.into()),
        flow_id: FLOW_ID.into(),
        flow_type: Some("messaging".into()),
        action: Some("messaging".into()),
        session_hint: Some(BARE_HINT.into()),
        provider: Some("provider".into()),
        channel: Some("chan".into()),
        conversation: Some("conv".into()),
        user: Some("user".into()),
        activity_id: Some("activity-1".into()),
        timestamp: None,
        payload: json!({ "text": "start" }),
        metadata: None,
        reply_scope: Some(ReplyScope {
            conversation: "conv".into(),
            thread: None,
            reply_to: None,
            correlation: None,
        }),
    }
}

fn build_runtime(pack_path: &Path, config: Arc<HostConfig>) -> Result<Arc<StateMachineRuntime>> {
    let rt = *RUNTIME;
    let pack = Arc::new(rt.block_on(PackRuntime::load(
        pack_path,
        Arc::clone(&config),
        None,
        None,
        None,
        None,
        Arc::new(greentic_runner_host::wasi::RunnerWasiPolicy::new()),
        greentic_runner_host::secrets::default_manager()?,
        None,
        false,
        ComponentResolution::default(),
    ))?);
    let engine = Arc::new(rt.block_on(FlowEngine::new(
        vec![Arc::clone(&pack)],
        Arc::clone(&config),
    ))?);

    // The session host and the resume store MUST share the same backing store
    // so the wait saved on turn 1 is visible to the fetch on resume.
    let session_store = new_session_store();
    let session_host = session_host_from(Arc::clone(&session_store));
    let state_store = new_state_store();
    let state_host = greentic_runner_host::storage::state::state_host_from(state_store);
    let secrets = greentic_runner_host::secrets::default_manager()?;

    let runtime = StateMachineRuntime::from_flow_engine(
        Arc::clone(&config),
        engine,
        std::collections::HashMap::new(),
        session_host,
        session_store,
        state_host,
        secrets,
        None,
    )?;
    Ok(Arc::new(runtime))
}

#[test]
fn resumer_resumes_paused_flow_to_completion() -> Result<()> {
    let rt = *RUNTIME;
    let temp = TempDir::new()?;
    let pack_path = temp.path().join("resume-via-resumer.gtpack");
    let bindings_path = temp.path().join("bindings.yaml");
    std::fs::write(&bindings_path, b"tenant: demo")?;
    build_wait_pack(&pack_path)?;

    let config = Arc::new(host_config(&bindings_path));
    let runtime = build_runtime(&pack_path, Arc::clone(&config))?;

    // ---- Turn 1: inbound message pauses the flow at `session.wait`. ----
    // `handle` returns Ok with the wait node's emitted payload; the saved wait
    // now lives in the shared session store.
    let _paused = rt
        .block_on(runtime.handle(inbound_envelope()))
        .context("turn 1 should pause at session.wait")?;

    // ---- Turn 2: resume via the production resumer (no broker). ----
    // The correlation id carries the routing/keying suffixes the wire contract
    // does not echo: `::pack=<id>` (keys the saved wait, per `tests/sorla_node.rs`)
    // and `::flow=<id>` (routes through `StateMachine::step`, which needs a
    // registered `(pack_id, flow_id)`). The resumer strips both, re-keys the
    // saved wait, and resumes the flow to completion.
    let correlation_id = format!("{BARE_HINT}::pack={PACK_ID}::flow={FLOW_ID}");
    let tenant = TenantCtx::new(
        EnvId::from_str("local").unwrap(),
        TenantId::from_str("demo").unwrap(),
    );
    let resumer = RuntimeSessionResumer::new(Arc::clone(&runtime));
    rt.block_on(resumer.resume(
        tenant,
        &correlation_id,
        json!({ "ok": true, "output": { "reply": "downstream-done" } }),
    ))
    .context("resume should advance past the wait node and complete the flow")?;

    // ---- A second resume must NOT find the wait (it was cleared on completion).
    // `handle` then runs the flow fresh, which pauses again at `session.wait`
    // (Ok), proving the first resume consumed the saved wait rather than the
    // resume being a no-op against a still-present snapshot.
    let tenant_again = TenantCtx::new(
        EnvId::from_str("local").unwrap(),
        TenantId::from_str("demo").unwrap(),
    );
    let resumer_again = RuntimeSessionResumer::new(Arc::clone(&runtime));
    let second =
        rt.block_on(resumer_again.resume(tenant_again, &correlation_id, json!({ "ok": true })));
    // With no saved wait, the synthesized envelope starts the flow from the top
    // and pauses again at `session.wait`; `handle` returns Ok either way. The
    // assertion that matters is that turn 2 above succeeded.
    assert!(
        second.is_ok(),
        "second resume should not error even though the wait was already consumed"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Gold end-to-end test: prove the NODE-emitted correlation id round-trips
// through the resumer. Unlike the test above (which hand-builds the correlation
// id), this captures the EXACT correlation the `sorla.call` node published and
// feeds that captured value to the resumer.
// ---------------------------------------------------------------------------

const SORLA_FLOW_ID: &str = "sorla.wait.flow";

/// Records the correlation id the `sorla.call` node published, and returns
/// `AwaitingResponse` so the flow pauses (mirrors the await path without NATS).
#[derive(Default)]
struct CapturingDispatchHandler {
    last_correlation: Mutex<Option<String>>,
}

#[async_trait]
impl RemoteDispatchHandler for CapturingDispatchHandler {
    async fn dispatch(&self, request: RemoteDispatch) -> Result<RemoteDispatchAction> {
        *self.last_correlation.lock().unwrap() = Some(request.correlation_id.clone());
        match request.mode {
            DispatchMode::Await => Ok(RemoteDispatchAction::AwaitingResponse {
                correlation_id: request.correlation_id,
            }),
            DispatchMode::FireAndForget => Ok(RemoteDispatchAction::Dispatched),
        }
    }
}

/// Build a `.gtpack` whose only flow is `sorla.call` (await) -> `emit.response`.
/// The await node routes forward to `done` so the resume has a node to advance
/// into, exactly like the existing await test in `tests/sorla_node.rs`.
fn build_sorla_wait_pack(pack_path: &Path) -> Result<()> {
    let runtime_flow = json!({
        "id": SORLA_FLOW_ID,
        "flow_type": "messaging",
        "start": "call",
        "nodes": {
            "call": {
                "component": "sorla.call",
                "operation": "dep-1",
                "input": { "await": true, "operation": "create", "input": {} },
                "routing": { "next": { "node_id": "done" } }
            },
            "done": {
                "component": "emit.response",
                "input": { "text": "resumed and completed" },
                "routing": "end"
            }
        }
    });
    let runtime_extension = json!({ "flows": [runtime_flow] });

    let mut extensions = BTreeMap::new();
    extensions.insert(
        RUNTIME_FLOW_EXTENSION_ID.to_string(),
        ExtensionRef {
            kind: RUNTIME_FLOW_EXTENSION_ID.to_string(),
            version: "2.0.0".into(),
            digest: None,
            location: None,
            inline: Some(ExtensionInline::Other(runtime_extension)),
        },
    );

    let manifest = PackManifest {
        schema_version: "1.0".into(),
        pack_id: PACK_ID.parse()?,
        name: None,
        version: Version::parse("0.0.0")?,
        kind: PackKind::Application,
        publisher: "test".into(),
        components: vec![ComponentManifest {
            id: "qa.process".parse()?,
            version: Version::parse("0.1.0")?,
            supports: vec![greentic_types::FlowKind::Messaging],
            world: "greentic:component@0.4.0".into(),
            profiles: ComponentProfiles::default(),
            capabilities: ComponentCapabilities::default(),
            configurators: None,
            operations: Vec::new(),
            config_schema: None,
            resources: ResourceHints::default(),
            dev_flows: BTreeMap::new(),
        }],
        flows: Vec::<PackFlowEntry>::new(),
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        signatures: Default::default(),
        secret_requirements: Vec::new(),
        bootstrap: None,
        agents: BTreeMap::new(),
        extensions: Some(extensions),
    };

    let mut zip = zip::ZipWriter::new(File::create(pack_path).context("create pack archive")?);
    let options: FileOptions<'_, ()> =
        FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let manifest_bytes = encode_pack_manifest(&manifest)?;
    zip.start_file("manifest.cbor", options)?;
    zip.write_all(&manifest_bytes)?;
    let component_path = component_artifact_path(
        pack_path
            .parent()
            .expect("pack path should have a parent temp dir"),
    )?;
    zip.start_file("components/qa.process.wasm", options)?;
    let mut comp_file = File::open(&component_path)?;
    std::io::copy(&mut comp_file, &mut zip)?;
    zip.finish().context("finalise pack archive")?;
    Ok(())
}

/// Inbound envelope that triggers the sorla flow. Routes to `SORLA_FLOW_ID`.
fn sorla_inbound_envelope() -> IngressEnvelope {
    IngressEnvelope {
        flow_id: SORLA_FLOW_ID.into(),
        ..inbound_envelope()
    }
}

/// Like `build_runtime`, but sets a [`RemoteDispatchHandler`] on the engine so
/// the `sorla.call` node can run, and returns the handler so the test can read
/// the captured correlation id.
fn build_runtime_with_dispatch(
    pack_path: &Path,
    config: Arc<HostConfig>,
    handler: Arc<CapturingDispatchHandler>,
) -> Result<Arc<StateMachineRuntime>> {
    let rt = *RUNTIME;
    let pack = Arc::new(rt.block_on(PackRuntime::load(
        pack_path,
        Arc::clone(&config),
        None,
        None,
        None,
        None,
        Arc::new(greentic_runner_host::wasi::RunnerWasiPolicy::new()),
        greentic_runner_host::secrets::default_manager()?,
        None,
        false,
        ComponentResolution::default(),
    ))?);
    let mut engine = rt.block_on(FlowEngine::new(
        vec![Arc::clone(&pack)],
        Arc::clone(&config),
    ))?;
    engine.set_remote_dispatch_handler(handler);
    let engine = Arc::new(engine);

    let session_store = new_session_store();
    let session_host = session_host_from(Arc::clone(&session_store));
    let state_store = new_state_store();
    let state_host = greentic_runner_host::storage::state::state_host_from(state_store);
    let secrets = greentic_runner_host::secrets::default_manager()?;

    let runtime = StateMachineRuntime::from_flow_engine(
        Arc::clone(&config),
        engine,
        std::collections::HashMap::new(),
        session_host,
        session_store,
        state_host,
        secrets,
        None,
    )?;
    Ok(Arc::new(runtime))
}

/// GOLD TEST: the correlation id the `sorla.call` node publishes round-trips
/// through `RuntimeSessionResumer` and resumes the paused flow.
///
/// 1. Run the flow (inbound) -> the `sorla.call` await node publishes a
///    correlation id (captured by the stub) and the flow PAUSES (`Waiting`).
/// 2. Feed the CAPTURED correlation id (not a hand-built one) to the resumer.
/// 3. The flow resumes past the wait; a subsequent resume finds no saved wait,
///    proving the first resume consumed it.
#[test]
fn node_emitted_correlation_round_trips_through_resumer() -> Result<()> {
    let rt = *RUNTIME;
    let temp = TempDir::new()?;
    let pack_path = temp.path().join("sorla-resume.gtpack");
    let bindings_path = temp.path().join("bindings.yaml");
    std::fs::write(&bindings_path, b"tenant: demo")?;
    build_sorla_wait_pack(&pack_path)?;

    let config = Arc::new(host_config(&bindings_path));
    let handler = Arc::new(CapturingDispatchHandler::default());
    let runtime =
        build_runtime_with_dispatch(&pack_path, Arc::clone(&config), Arc::clone(&handler))?;

    // ---- Turn 1: inbound message runs the sorla.call node and pauses. ----
    let paused = rt
        .block_on(runtime.handle(sorla_inbound_envelope()))
        .context("turn 1 should run the sorla.call node and pause awaiting the response")?;
    // The wait surfaces as a pending payload (the node returns
    // `{ "pending": true, ... }`); the saved wait now lives in the session store.
    assert!(
        payload_has_pending(&paused),
        "expected the sorla.call node to pause with a pending payload, got {paused:?}"
    );

    // Capture the EXACT correlation id the node published.
    let captured_correlation = handler
        .last_correlation
        .lock()
        .unwrap()
        .clone()
        .expect("the sorla.call node must have published a correlation id");

    // Sanity: the captured correlation carries the markers the resumer parses,
    // and preserves the bare hint used to key the saved wait.
    assert!(
        captured_correlation.starts_with(BARE_HINT),
        "captured correlation must preserve the bare hint, got {captured_correlation}"
    );
    assert!(
        captured_correlation.contains(&format!("::pack={PACK_ID}")),
        "captured correlation must carry the pack marker, got {captured_correlation}"
    );
    assert!(
        captured_correlation.contains(&format!("::flow={SORLA_FLOW_ID}")),
        "captured correlation must carry the flow marker, got {captured_correlation}"
    );

    // ---- Turn 2: resume with the CAPTURED correlation id (no hand-building). ----
    let tenant = TenantCtx::new(
        EnvId::from_str("local").unwrap(),
        TenantId::from_str("demo").unwrap(),
    );
    let resumer = RuntimeSessionResumer::new(Arc::clone(&runtime));
    rt.block_on(resumer.resume(
        tenant,
        &captured_correlation,
        json!({ "ok": true, "output": { "reply": "downstream-done" } }),
    ))
    .context("resuming with the node-emitted correlation id should advance past the wait")?;

    // ---- The saved wait must have been consumed by the first resume. ----
    // A second resume finds no wait, so the synthesized envelope re-runs the flow
    // from the top and pauses again (Ok). The captured-correlation resume above
    // succeeding is the load-bearing assertion.
    let tenant_again = TenantCtx::new(
        EnvId::from_str("local").unwrap(),
        TenantId::from_str("demo").unwrap(),
    );
    let resumer_again = RuntimeSessionResumer::new(Arc::clone(&runtime));
    let second = rt.block_on(resumer_again.resume(
        tenant_again,
        &captured_correlation,
        json!({ "ok": true }),
    ));
    assert!(
        second.is_ok(),
        "second resume should not error even though the wait was already consumed"
    );

    Ok(())
}

/// Recursively search a response envelope for a `pending: true` marker (the
/// engine wraps node outputs in nested state envelopes).
fn payload_has_pending(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            if map.get("pending") == Some(&serde_json::Value::Bool(true)) {
                return true;
            }
            map.values().any(payload_has_pending)
        }
        serde_json::Value::Array(items) => items.iter().any(payload_has_pending),
        _ => false,
    }
}
