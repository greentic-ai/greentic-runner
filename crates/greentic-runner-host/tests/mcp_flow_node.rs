//! End-to-end characterization of the `component == "mcp"` flow node
//! (LOCKED ENCODING v2: `server`/`tool`/`arguments`/`output` carried in the
//! node payload/config).
//!
//! This is the flow-execution MCP path (role `flow_editor`), separate from the
//! agent-loop MCP path (role `agentic_worker`). A single-node `.gtpack` flow is
//! built and run through the real [`FlowEngine`], with the per-tenant MCP
//! catalog backed by two wiremock servers: a fake admin
//! (`/api/v1/designer/tenant/me/mcp-servers`) returning one `flow_editor` MCP
//! server, and a fake MCP server speaking the JSON-RPC contract (initialize,
//! initialized, tools/list, tools/call).
//!
//! `FlowEngine::new` constructs the MCP source from the admin endpoint and
//! token env vars, so the success test points those at the admin wiremock. The
//! wiremock admin/MCP mount shapes mirror the in-module tests in the
//! `greentic-aw-runtime` mcp_source module (those helpers are test-only and
//! cannot be reused from here).
//!
//! Two paths are covered. The happy path calls the tool with templated
//! arguments and binds its structured result under the configured `output`
//! state key. The degraded path leaves the admin unconfigured (no env), so the
//! node FAILS GRACEFULLY with a structured error value while the flow still
//! COMPLETES (no panic, no aborted runtime).

#![cfg(feature = "agentic-worker")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use greentic_runner_host::config::{
    FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
    WebhookPolicy,
};
use greentic_runner_host::pack::{ComponentResolution, PackRuntime};
use greentic_runner_host::runner::engine::{FlowContext, FlowEngine, FlowStatus};
use greentic_runner_host::trace::TraceConfig;
use greentic_runner_host::validate::ValidationConfig;
use greentic_types::{
    ExtensionInline, ExtensionRef, PackFlowEntry, PackKind, PackManifest, encode_pack_manifest,
};
use once_cell::sync::Lazy;
use semver::Version;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::FileOptions;

const RUNTIME_FLOW_EXTENSION_ID: &str = "greentic.pack.runtime_flow";
const PACK_ID: &str = "mcp.flow.node.test";
const FLOW_ID: &str = "mcp.flow";
const SESSION_HINT: &str = "demo:provider:chan:conv:user";

static RUNTIME: Lazy<&'static tokio::runtime::Runtime> = Lazy::new(|| {
    Box::leak(Box::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime"),
    ))
});

/// Serializes the env-dependent tests in this file: both read/write the shared
/// process-global `GREENTIC_AW_*` vars that `FlowEngine::new` consumes.
static ENV_GUARD: Mutex<()> = Mutex::new(());

// ── wiremock fakes (shapes mirror mcp_source.rs in-module tests) ──────────────

fn initialize_ok() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("Mcp-Session-Id", "sess-1")
        .set_body_json(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "protocolVersion": "2025-06-18",
                "serverInfo": { "name": "fake", "version": "1.0.0" }
            }
        }))
}

/// Mount the 4-call MCP JSON-RPC contract on a fresh wiremock server, returning
/// `call_result_json` for `tools/call`.
async fn fake_mcp_server(call_result_json: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "initialize" })))
        .respond_with(initialize_ok())
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({ "method": "notifications/initialized" }),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/list" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 2,
            "result": { "tools": [
                { "name": "get_issue", "description": "Get an issue",
                  "inputSchema": { "type": "object",
                    "properties": { "id": { "type": "string" } } } }
            ] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({ "method": "tools/call" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 3,
            "result": call_result_json
        })))
        .mount(&server)
        .await;
    server
}

/// Mount the admin `mcp-servers` endpoint returning one `flow_editor` server
/// pointing at `mcp_url`.
async fn fake_admin(mcp_url: &str) -> MockServer {
    let admin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/designer/tenant/me/mcp-servers"))
        .and(header("authorization", "Bearer gtc_live_test"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "servers": [
                {
                    "id": "github", "name": "GitHub", "transport_url": mcp_url,
                    "auth_header_name": null, "auth_token": null,
                    "allowed_tools": null, "roles": ["flow_editor"]
                }
            ]
        })))
        .mount(&admin)
        .await;
    admin
}

// ── pack harness (mirrors runtime_call_nodes.rs build_dispatch_pack) ──────────

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
        agents: std::collections::HashMap::new(),
        graphs: std::collections::HashMap::new(),
    }
}

/// Build a `.gtpack` whose only flow is a single `component == "mcp"` node
/// (LOCKED ENCODING v2). `node_input` carries `{ server, tool, arguments,
/// output? }` directly in the node payload/config — `server`/`tool` are the
/// source of truth, NOT an `operation` string. No WASM component is needed:
/// the node is a native runtime dispatch.
///
/// `component = "mcp"` is a valid `greentic_types::ComponentId`, so the
/// runtime-flow pack round-trips it through `ComponentId::from_str` without the
/// earlier `:`/`/` rejection; the flow adapter sees exactly `"mcp"`.
fn build_mcp_pack(pack_path: &Path, node_input: Value) -> Result<()> {
    let mut nodes = serde_json::Map::new();
    nodes.insert(
        "call".to_string(),
        json!({
            "component": "mcp",
            "input": node_input,
            "routing": "end",
        }),
    );

    let runtime_flow = json!({
        "id": FLOW_ID,
        "flow_type": "messaging",
        "start": "call",
        "nodes": Value::Object(nodes),
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
        components: Vec::new(),
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
    zip.finish().context("finalise pack archive")?;
    Ok(())
}

fn build_engine(
    pack_path: &Path,
    config: Arc<HostConfig>,
) -> Result<(Arc<PackRuntime>, FlowEngine)> {
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
    let engine = rt.block_on(FlowEngine::new(
        vec![Arc::clone(&pack)],
        Arc::clone(&config),
    ))?;
    Ok((pack, engine))
}

fn flow_ctx<'a>(config: &'a HostConfig, pack_id: &'a str) -> FlowContext<'a> {
    FlowContext {
        tenant: config.tenant.as_str(),
        pack_id,
        flow_id: FLOW_ID,
        node_id: None,
        tool: None,
        action: Some("messaging"),
        session_id: Some(SESSION_HINT),
        provider_id: Some("provider"),
        reply_scope: None,
        retry_config: config.retry.clone().into(),
        attempt: 1,
        observer: None,
        mocks: None,
    }
}

/// Recursively find a value at `key` anywhere in the (possibly envelope-wrapped)
/// output tree.
fn find_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values().find_map(|v| find_key(v, key))
        }
        Value::Array(items) => items.iter().find_map(|v| find_key(v, key)),
        _ => None,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[test]
fn mcp_node_calls_tool_and_binds_output() -> Result<()> {
    let _guard = ENV_GUARD.lock().unwrap();
    let rt = *RUNTIME;
    let temp = TempDir::new()?;
    let pack_path = temp.path().join("mcp-ok.gtpack");
    let bindings_path = temp.path().join("bindings.yaml");
    std::fs::write(&bindings_path, b"tenant: demo")?;

    // Stand up the wiremock pair: MCP server returns a structured payload.
    let mcp = rt.block_on(fake_mcp_server(
        json!({ "structuredContent": { "title": "Bug" } }),
    ));
    let admin = rt.block_on(fake_admin(&mcp.uri()));

    // LOCKED ENCODING v2: the component ref the flow adapter parses is exactly
    // `"mcp"` — a valid `ComponentId` (no `:`/`/` to be rejected). This is what
    // `runtime_flow_to_flow` runs through `ComponentId::from_str` at pack load.
    assert!(
        greentic_types::ComponentId::from_str("mcp").is_ok(),
        "component ref `mcp` must be a valid ComponentId"
    );

    // `arguments` uses a `{{ }}` template against entry state; `output` names
    // the state key the result is bound under. `server`/`tool` live in the
    // payload (the source of truth), NOT in an `operation` string.
    build_mcp_pack(
        &pack_path,
        json!({
            "server": "github",
            "tool": "get_issue",
            "arguments": { "id": "{{ entry.issue_id }}" },
            "output": "issue"
        }),
    )?;

    let config = Arc::new(host_config(&bindings_path));

    // SAFETY: serialized by ENV_GUARD; cleared after the run below.
    unsafe {
        std::env::set_var("GREENTIC_AW_ADMIN_ENDPOINT", admin.uri());
        std::env::set_var("GREENTIC_AW_ADMIN_TOKEN", "gtc_live_test");
        std::env::remove_var("GREENTIC_AW_MCP");
    }

    let (pack, engine) = build_engine(&pack_path, Arc::clone(&config))?;

    unsafe {
        std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
        std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
    }

    let ctx = flow_ctx(&config, pack.metadata().pack_id.as_str());
    let execution = rt
        .block_on(engine.execute(ctx, json!({ "issue_id": "42" })))
        .context("mcp flow run")?;

    match execution.status {
        FlowStatus::Completed => {}
        FlowStatus::Waiting(wait) => anyhow::bail!("flow paused unexpectedly: {:?}", wait.reason),
    }

    // The structured tool result is bound under the `issue` output key.
    let issue = find_key(&execution.output, "issue")
        .with_context(|| format!("output key `issue` missing, got {:?}", execution.output))?;
    assert_eq!(
        issue,
        &json!({ "title": "Bug" }),
        "bound output mismatch, got {:?}",
        execution.output
    );
    // And no error surfaced.
    assert!(
        find_key(&execution.output, "error").is_none(),
        "unexpected error in output: {:?}",
        execution.output
    );
    Ok(())
}

#[test]
fn mcp_node_degrades_gracefully_when_unconfigured() -> Result<()> {
    let _guard = ENV_GUARD.lock().unwrap();
    let rt = *RUNTIME;
    let temp = TempDir::new()?;
    let pack_path = temp.path().join("mcp-degraded.gtpack");
    let bindings_path = temp.path().join("bindings.yaml");
    std::fs::write(&bindings_path, b"tenant: demo")?;

    build_mcp_pack(
        &pack_path,
        json!({
            "server": "github",
            "tool": "get_issue",
            "arguments": { "id": "1" },
            "output": "issue"
        }),
    )?;

    let config = Arc::new(host_config(&bindings_path));

    // No admin env configured -> MCP source is None -> graceful node error.
    unsafe {
        std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
        std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
        std::env::remove_var("GREENTIC_AW_MCP");
    }

    let (pack, engine) = build_engine(&pack_path, Arc::clone(&config))?;

    let ctx = flow_ctx(&config, pack.metadata().pack_id.as_str());
    let execution = rt
        .block_on(engine.execute(ctx, json!({})))
        .context("mcp degraded flow run")?;

    // The runtime must NOT abort: the flow completes...
    match execution.status {
        FlowStatus::Completed => {}
        FlowStatus::Waiting(wait) => anyhow::bail!("flow paused unexpectedly: {:?}", wait.reason),
    }

    // ...and the node binds a structured error under the `issue` output key.
    let bound = find_key(&execution.output, "issue")
        .with_context(|| format!("output key `issue` missing, got {:?}", execution.output))?;
    let err = bound
        .get("error")
        .and_then(Value::as_str)
        .with_context(|| format!("expected an `error` string under `issue`, got {bound:?}"))?;
    assert!(
        err.contains("MCP is not configured"),
        "unexpected error message: {err}"
    );
    Ok(())
}
