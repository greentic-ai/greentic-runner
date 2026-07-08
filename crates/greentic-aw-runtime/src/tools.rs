//! Tool resolution + dispatch helpers.
//!
//! `ExtensionRuntime::invoke_tool` is a synchronous `fn` performing
//! Wasmtime WASM dispatch (CPU-bound, may block seconds). Every call
//! site MUST wrap it in `tokio::task::spawn_blocking` (spec §5.3). This
//! module calls it for agent tool dispatch; `llm_extension::RuntimeInvoker`
//! also calls it (likewise wrapped) when the LLM runs through an extension.
//!
//! Each tool call is recorded in Redis by `tool_call_id` BEFORE the
//! result is committed so a state-save failure cannot cause a
//! double-dispatch on the next `step()` (idempotency ledger).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use greentic_ext_runtime::ExtensionRuntime;
use greentic_ext_runtime::host_ports::HostCallContext;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};

use crate::component_source::ComponentToolCatalog;
use crate::config::ToolRef;
use crate::error::{AgentError, StateError};
use crate::flow_source::FlowToolCatalog;
use crate::kv::AwKv;
use crate::llm::LlmToolSchema;
use crate::mcp_source::McpToolCatalog;
use crate::state::ToolCallRecord;
use crate::tenant::TenantContext;

/// Whether the agent may call this tool — exact (extension_id, tool_name) match.
pub fn is_tool_allowed(call: &ToolCallRecord, allowed: &[ToolRef]) -> bool {
    allowed
        .iter()
        .any(|t| t.extension_id == call.extension_id && t.tool_name == call.tool_name)
}

/// Map allow-listed tools to LLM-facing schemas via
/// `ExtensionRuntime::list_tools`. Tools whose extension is not loaded,
/// or that the extension doesn't expose, are logged and skipped (the
/// LLM simply won't see them). `input_schema_json` is parsed into a
/// JSON Value for the LLM tool parameters; on parse failure an empty
/// object schema is used.
///
/// Tools whose `extension_id` starts with `"mcp:"` are resolved from the
/// per-tenant [`McpToolCatalog`] (`mcp`) instead of the extension runtime: the
/// suffix after `"mcp:"` is the MCP `server_id`, and the catalog supplies the
/// LLM-facing `description`/`parameters`. An mcp ref with no matching catalog
/// entry (or no catalog at all) is logged and dropped, mirroring the
/// extension-runtime "tool not found" path.
///
/// Tools whose `extension_id` starts with `"component:"` are resolved the same
/// way from the per-tenant [`ComponentToolCatalog`] (`components`): the suffix
/// is the `component_ref` and `tool_name` the operation, and the catalog
/// supplies the operation's `description`/`parameters`. A `component:` ref with
/// no matching catalog entry (or no catalog) is likewise logged and dropped.
///
/// Tools whose `extension_id` starts with `"flow:"` are resolved from the
/// per-tenant [`FlowToolCatalog`] (`flows`): the suffix after `"flow:"` is the
/// `flow_ref`, which is the sole key (no operation). The catalog supplies the
/// LLM-facing `description`/`parameters`. A `flow:` ref with no matching catalog
/// entry (or no catalog) is likewise logged and dropped.
pub fn list_tools_for_llm(
    ext_runtime: &ExtensionRuntime,
    mcp: Option<&McpToolCatalog>,
    components: Option<&ComponentToolCatalog>,
    flows: Option<&FlowToolCatalog>,
    allowed: &[ToolRef],
) -> Vec<LlmToolSchema> {
    let mut out = Vec::with_capacity(allowed.len());
    for t in allowed {
        if let Some(server_id) = t.extension_id.strip_prefix("mcp:") {
            match mcp.and_then(|c| c.tool_entry(server_id, &t.tool_name)) {
                Some(entry) => out.push(LlmToolSchema {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    description: entry.description.clone(),
                    parameters: entry.parameters.clone(),
                }),
                None => tracing::warn!(
                    extension = %t.extension_id, tool = %t.tool_name,
                    "mcp tool not found in catalog; dropping from LLM tool list"
                ),
            }
            continue;
        }
        if let Some(component_ref) = t.extension_id.strip_prefix("component:") {
            match components.and_then(|c| c.tool_entry(component_ref, &t.tool_name)) {
                Some(entry) => out.push(LlmToolSchema {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    description: entry.description.clone(),
                    parameters: entry.parameters.clone(),
                }),
                None => tracing::warn!(
                    extension = %t.extension_id, tool = %t.tool_name,
                    "component tool not found in catalog; dropping from LLM tool list"
                ),
            }
            continue;
        }
        if let Some(flow_ref) = t.extension_id.strip_prefix("flow:") {
            let entry = flows.and_then(|c| c.tool_entry(flow_ref));
            let description = t
                .description
                .clone()
                .or_else(|| entry.map(|e| e.description.clone()));
            let parameters = t
                .input_schema
                .clone()
                .or_else(|| entry.map(|e| e.parameters.clone()));
            match (description, parameters) {
                (Some(description), Some(parameters)) => out.push(LlmToolSchema {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    description,
                    parameters,
                }),
                _ => tracing::warn!(
                    extension = %t.extension_id, tool = %t.tool_name,
                    "flow tool has neither an author contract nor a catalog entry; dropping from LLM tool list"
                ),
            }
            continue;
        }
        match ext_runtime.list_tools(&t.extension_id) {
            Ok(defs) => {
                if let Some(def) = defs.into_iter().find(|d| d.name == t.tool_name) {
                    let parameters: serde_json::Value = serde_json::from_str(
                        &def.input_schema_json,
                    )
                    .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));
                    out.push(LlmToolSchema {
                        extension_id: t.extension_id.clone(),
                        tool_name: t.tool_name.clone(),
                        description: def.description,
                        parameters,
                    });
                } else {
                    tracing::warn!(
                        extension = %t.extension_id, tool = %t.tool_name,
                        "tool not found in extension; dropping from LLM tool list"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    extension = %t.extension_id, error = %e,
                    "extension list_tools failed; skipping"
                );
            }
        }
    }
    out
}

/// A tool an agent declared that will NOT be visible to the LLM, with a
/// human-readable reason. Produced by [`missing_tools`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingTool {
    pub extension_id: String,
    pub tool_name: String,
    pub reason: String,
}

/// Preflight check: of the `allowed` tools an agent declares, return those that
/// cannot be resolved to a live LLM schema, each with a reason.
///
/// This mirrors the resolution logic in [`list_tools_for_llm`] exactly, but
/// reports failures instead of dropping them silently. Callers use it to warn
/// the operator at startup — otherwise an agent whose tools all failed to load
/// runs with an empty tool set and hallucinates tool results.
pub fn missing_tools(
    ext_runtime: &ExtensionRuntime,
    mcp: Option<&McpToolCatalog>,
    components: Option<&ComponentToolCatalog>,
    flows: Option<&FlowToolCatalog>,
    allowed: &[ToolRef],
) -> Vec<MissingTool> {
    let mut missing = Vec::new();
    for t in allowed {
        if let Some(server_id) = t.extension_id.strip_prefix("mcp:") {
            if mcp
                .and_then(|c| c.tool_entry(server_id, &t.tool_name))
                .is_none()
            {
                missing.push(MissingTool {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    reason: "MCP tool not found in the tenant catalog".to_string(),
                });
            }
            continue;
        }
        if let Some(component_ref) = t.extension_id.strip_prefix("component:") {
            if components
                .and_then(|c| c.tool_entry(component_ref, &t.tool_name))
                .is_none()
            {
                missing.push(MissingTool {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    reason: "component tool not found in the catalog".to_string(),
                });
            }
            continue;
        }
        if let Some(flow_ref) = t.extension_id.strip_prefix("flow:") {
            if flows.and_then(|c| c.tool_entry(flow_ref)).is_none() {
                missing.push(MissingTool {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    reason: "flow tool not found in the catalog".to_string(),
                });
            }
            continue;
        }
        match ext_runtime.list_tools(&t.extension_id) {
            Ok(defs) => {
                if !defs.iter().any(|d| d.name == t.tool_name) {
                    missing.push(MissingTool {
                        extension_id: t.extension_id.clone(),
                        tool_name: t.tool_name.clone(),
                        reason: "extension loaded but does not expose this tool".to_string(),
                    });
                }
            }
            Err(e) => {
                missing.push(MissingTool {
                    extension_id: t.extension_id.clone(),
                    tool_name: t.tool_name.clone(),
                    reason: format!("extension failed to load: {e}"),
                });
            }
        }
    }
    missing
}

/// Build a [`HostCallContext`] from the per-step [`TenantContext`].
///
/// The extension host (e.g. the designer's `DesignerLlmBridge`) uses
/// `ctx.tenant` to resolve the LLM provider per-tenant and `ctx.user_email`
/// for optional per-user override (present only in interactive test-chat steps;
/// `None` for autonomous workers).
pub(crate) fn host_ctx_from_tenant(t: &TenantContext) -> HostCallContext {
    HostCallContext {
        tenant: if t.tenant_id.is_empty() {
            None
        } else {
            Some(t.tenant_id.clone())
        },
        user_email: t.user_email.clone(),
    }
}

/// Dispatch a single tool call. Wraps the blocking `invoke_tool_ctx` in
/// `tokio::task::spawn_blocking` so the async executor thread is never
/// stalled. Returns the tool result as a JSON Value.
///
/// Calls whose `extension_id` starts with `"mcp:"` route through the
/// per-tenant [`McpToolCatalog`] (`mcp`) instead: the suffix is the MCP
/// `server_id`, and dispatch goes over HTTP via
/// [`crate::mcp_source::dispatch_route`]. An MCP call NEVER yields `Err` — a
/// missing route or remote failure is surfaced as an `{"error": ...}` value so
/// the LLM observes it as a normal tool result.
///
/// Calls whose `extension_id` starts with `"component:"` route through the
/// per-tenant [`ComponentToolCatalog`] (`components`): the suffix is the
/// `component_ref` and dispatch goes to the host component invoker via
/// [`ComponentToolCatalog::dispatch`]. Like the mcp path it NEVER yields `Err`
/// — an unknown operation or a missing catalog becomes an `{"error": ...}`
/// value. Other ids keep the existing blocking WASM path.
pub async fn dispatch_tool_call(
    ext_runtime: Arc<ExtensionRuntime>,
    mcp: Option<Arc<McpToolCatalog>>,
    components: Option<Arc<ComponentToolCatalog>>,
    flows: Option<Arc<FlowToolCatalog>>,
    call: ToolCallRecord,
    tenant: &TenantContext,
) -> Result<serde_json::Value, AgentError> {
    if let Some(server_id) = call.extension_id.strip_prefix("mcp:") {
        let value = match mcp
            .as_deref()
            .and_then(|c| c.route(server_id, &call.tool_name))
        {
            Some(route) => {
                let args = call.args.to_string();
                crate::mcp_source::dispatch_route(route, &args).await
            }
            None => {
                tracing::warn!(
                    server = %server_id,
                    tool = %call.tool_name,
                    "mcp call has no route in the tenant catalog; returning error value"
                );
                serde_json::json!({
                    "error": format!("unknown mcp tool '{}/{}'", server_id, call.tool_name)
                })
            }
        };
        return Ok(value);
    }

    if let Some(component_ref) = call.extension_id.strip_prefix("component:") {
        let value = match components.as_deref() {
            Some(cat) => {
                let args = call.args.to_string();
                cat.dispatch(component_ref, &call.tool_name, &args).await
            }
            None => {
                tracing::warn!(
                    component = %component_ref,
                    tool = %call.tool_name,
                    "component call has no catalog wired; returning error value"
                );
                serde_json::json!({
                    "error": format!(
                        "unknown component tool '{}/{}'",
                        component_ref, call.tool_name
                    )
                })
            }
        };
        return Ok(value);
    }

    if let Some(flow_ref) = call.extension_id.strip_prefix("flow:") {
        let value = match flows.as_deref() {
            Some(cat) => cat.dispatch(flow_ref, &call.args.to_string()).await,
            None => {
                tracing::warn!(flow = %flow_ref, "flow call has no catalog wired; returning error value");
                serde_json::json!({ "error": format!("unknown flow tool '{flow_ref}'") })
            }
        };
        return Ok(value);
    }

    let args_json = call.args.to_string();
    let extension_id = call.extension_id.clone();
    let tool_name = call.tool_name.clone();
    let ctx = host_ctx_from_tenant(tenant);
    let raw = tokio::task::spawn_blocking(move || {
        ext_runtime.invoke_tool_ctx(&extension_id, &tool_name, &args_json, &ctx)
    })
    .await
    .map_err(|e| AgentError::ToolDispatch(format!("join: {e}")))?
    .map_err(|e| AgentError::ToolDispatch(format!("invoke: {e}")))?;
    serde_json::from_str(&raw).map_err(|e| AgentError::ToolDispatch(format!("decode: {e}")))
}

/// Idempotency ledger entry stored under
/// `aw:{tenant}:{env}:{session}:tool_calls:{call_id}` (TTL 7 days).
#[derive(Serialize, Deserialize, Clone)]
pub struct ToolLedgerEntry {
    pub result: serde_json::Value,
}

pub fn ledger_key(tenant: &TenantContext, session_id: &str, call_id: &str) -> String {
    format!("{}:{session_id}:tool_calls:{call_id}", tenant.key_prefix())
}

/// Idempotency ledger for tool calls. Records tool results keyed by
/// `tool_call_id` so a state-save failure does not cause a duplicate
/// dispatch (re-sending the same email, etc.) on the next `step()`.
///
/// Dyn-safe (`Arc<dyn ToolLedger>`); production uses [`RedisToolLedger`],
/// tests use `NoopToolLedger` (from the `test-mock` module).
pub trait ToolLedger: Send + Sync {
    /// Return a previously-recorded result for this call_id, or `None`.
    fn get<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        call_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<serde_json::Value>, StateError>> + Send + 'a>>;

    /// Record a tool result (TTL 7 days).
    fn record<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        call_id: &'a str,
        result: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>>;
}

const LEDGER_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Production tool ledger backed by a multiplexed `ConnectionManager`.
///
/// Shares the Redis instance with `RedisAgentStateStore` via
/// `RedisAgentStateStore::manager()`. The manager is `Clone` (cheap,
/// reference-counted) so per-call clones open no new connections.
pub struct RedisToolLedger {
    manager: ConnectionManager,
}

impl RedisToolLedger {
    pub fn new(manager: ConnectionManager) -> Self {
        Self { manager }
    }
}

impl ToolLedger for RedisToolLedger {
    fn get<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        call_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<serde_json::Value>, StateError>> + Send + 'a>>
    {
        Box::pin(async move {
            let key = ledger_key(tenant, session_id, call_id);
            let mut conn = self.manager.clone();
            let raw: Option<String> = conn
                .get(&key)
                .await
                .map_err(|e| StateError::Redis(format!("ledger get: {e}")))?;
            match raw {
                Some(json) => {
                    let entry: ToolLedgerEntry = serde_json::from_str(&json)
                        .map_err(|e| StateError::Decode(format!("ledger decode: {e}")))?;
                    Ok(Some(entry.result))
                }
                None => Ok(None),
            }
        })
    }

    fn record<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        call_id: &'a str,
        result: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
        Box::pin(async move {
            let key = ledger_key(tenant, session_id, call_id);
            let entry = ToolLedgerEntry { result };
            let json = serde_json::to_string(&entry)
                .map_err(|e| StateError::Decode(format!("ledger encode: {e}")))?;
            let mut conn = self.manager.clone();
            let _: () = conn
                .set_ex(&key, json, LEDGER_TTL_SECS)
                .await
                .map_err(|e| StateError::Redis(format!("ledger set_ex: {e}")))?;
            Ok(())
        })
    }
}

const KV_LEDGER_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Tool-call idempotency ledger over [`AwKv`] (Redis-free). Same key format
/// and 7-day TTL as [`RedisToolLedger`].
pub struct KvToolLedger {
    kv: Arc<dyn AwKv>,
}

impl KvToolLedger {
    pub fn new(kv: Arc<dyn AwKv>) -> Self {
        Self { kv }
    }
}

impl ToolLedger for KvToolLedger {
    fn get<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        call_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<serde_json::Value>, StateError>> + Send + 'a>>
    {
        Box::pin(async move {
            let key = ledger_key(tenant, session_id, call_id);
            match self.kv.get(&key).await? {
                Some(bytes) => {
                    let entry: ToolLedgerEntry = serde_json::from_slice(&bytes)
                        .map_err(|e| StateError::Decode(format!("ledger decode: {e}")))?;
                    Ok(Some(entry.result))
                }
                None => Ok(None),
            }
        })
    }

    fn record<'a>(
        &'a self,
        tenant: &'a TenantContext,
        session_id: &'a str,
        call_id: &'a str,
        result: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
        Box::pin(async move {
            let key = ledger_key(tenant, session_id, call_id);
            let bytes = serde_json::to_vec(&ToolLedgerEntry { result })
                .map_err(|e| StateError::Decode(format!("ledger encode: {e}")))?;
            self.kv.set_ex(&key, bytes, KV_LEDGER_TTL).await
        })
    }
}

#[cfg(test)]
mod ctx_tests {
    use super::*;
    use crate::tenant::TenantContext;

    #[test]
    fn host_ctx_carries_tenant_and_optional_user() {
        let c1 = host_ctx_from_tenant(&TenantContext::new("acme", "prod"));
        assert_eq!(c1.tenant.as_deref(), Some("acme"));
        assert_eq!(c1.user_email, None);
        let c2 = host_ctx_from_tenant(
            &TenantContext::new("acme", "prod").with_user_email(Some("u@x.com".into())),
        );
        assert_eq!(c2.user_email.as_deref(), Some("u@x.com"));
        let c3 = host_ctx_from_tenant(&TenantContext::new("", ""));
        assert_eq!(
            c3.tenant, None,
            "empty tenant_id must map to None, not Some(\"\")"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn is_tool_allowed_returns_true_for_exact_match() {
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            description: None,
            input_schema: None,
        }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            args: serde_json::json!({}),
        };
        assert!(is_tool_allowed(&call, &allowed));
    }

    #[test]
    fn is_tool_allowed_returns_false_for_unauthorized_tool() {
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            description: None,
            input_schema: None,
        }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "post".into(),
            args: serde_json::json!({}),
        };
        assert!(!is_tool_allowed(&call, &allowed));
    }

    #[test]
    fn ledger_key_includes_tenant_env_session_callid() {
        let tc = TenantContext::new("acme", "prod");
        let key = ledger_key(&tc, "sess-1", "call-abc");
        assert_eq!(key, "aw:acme:prod:sess-1:tool_calls:call-abc");
    }

    #[test]
    fn list_tools_for_llm_with_no_extensions_returns_empty() {
        // for_test runtime has no extensions loaded → list_tools errors
        // (NotFound) for every ext → all skipped → empty result.
        let rt = ExtensionRuntime::for_test();
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            description: None,
            input_schema: None,
        }];
        let schemas = list_tools_for_llm(&rt, None, None, None, &allowed);
        assert!(schemas.is_empty());
    }

    #[test]
    fn missing_tools_reports_unloaded_extension() {
        // No extensions loaded → the declared tool cannot resolve and is
        // reported as missing with a load-failure reason (instead of being
        // dropped silently, which is what causes hallucinated tool results).
        let rt = ExtensionRuntime::for_test();
        let allowed = vec![ToolRef {
            extension_id: "greentic.hubspot".into(),
            tool_name: "hubspot_contacts".into(),
            description: None,
            input_schema: None,
        }];
        let missing = missing_tools(&rt, None, None, None, &allowed);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].extension_id, "greentic.hubspot");
        assert_eq!(missing[0].tool_name, "hubspot_contacts");
        assert!(
            missing[0].reason.contains("failed to load"),
            "got: {}",
            missing[0].reason
        );
    }

    #[test]
    fn missing_tools_reports_mcp_tool_absent_from_catalog() {
        let rt = ExtensionRuntime::for_test();
        let allowed = vec![ToolRef {
            extension_id: "mcp:github".into(),
            tool_name: "create_issue".into(),
            description: None,
            input_schema: None,
        }];
        // No catalog provided → the mcp tool is unresolvable.
        let missing = missing_tools(&rt, None, None, None, &allowed);
        assert_eq!(missing.len(), 1);
        assert!(
            missing[0].reason.contains("MCP tool not found"),
            "got: {}",
            missing[0].reason
        );
    }

    use std::collections::HashMap;

    use crate::mcp_source::{McpRoute, McpToolCatalog, McpToolEntry, route_for_tests};

    /// Build a one-tool catalog whose route (when present) aims at
    /// `transport_url`. Pass `with_route = false` to register only the schema
    /// (list-side) without a dispatch route.
    fn catalog_with(
        server: &str,
        tool: &str,
        description: &str,
        parameters: serde_json::Value,
        transport_url: Option<&str>,
    ) -> McpToolCatalog {
        let mut tools: HashMap<(String, String), McpToolEntry> = HashMap::new();
        tools.insert(
            (server.to_string(), tool.to_string()),
            McpToolEntry {
                description: description.to_string(),
                parameters,
            },
        );
        let mut routes: HashMap<(String, String), McpRoute> = HashMap::new();
        if let Some(url) = transport_url {
            routes.insert(
                (server.to_string(), tool.to_string()),
                route_for_tests(server, tool, url),
            );
        }
        McpToolCatalog::for_tests(tools, routes)
    }

    /// Mount the minimal MCP JSON-RPC contract (initialize, initialized,
    /// tools/call) on a fresh wiremock server returning `call_result`.
    /// Replicated from `mcp_source` tests — only the few mount lines needed
    /// to exercise dispatch.
    async fn fake_mcp_call_server(call_result: serde_json::Value) -> wiremock::MockServer {
        use wiremock::matchers::{body_partial_json, method};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                serde_json::json!({ "method": "initialize" }),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "sess-1")
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "serverInfo": { "name": "fake", "version": "1.0.0" }
                        }
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                serde_json::json!({ "method": "notifications/initialized" }),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                serde_json::json!({ "method": "tools/call" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 3,
                "result": call_result
            })))
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn mcp_ref_listed_from_catalog() {
        // A catalog-backed mcp: ref is emitted as an LlmToolSchema with the
        // catalog's description/parameters; the ext_runtime is never consulted
        // for it (the for_test runtime has no extensions loaded).
        let rt = ExtensionRuntime::for_test();
        let params = serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } }
        });
        let catalog = catalog_with("s1", "get_issue", "Get an issue", params.clone(), None);

        let allowed = vec![
            ToolRef {
                extension_id: "mcp:s1".into(),
                tool_name: "get_issue".into(),
                description: None,
                input_schema: None,
            },
            // Absent from the catalog → dropped (warn), not panicked.
            ToolRef {
                extension_id: "mcp:s1".into(),
                tool_name: "missing".into(),
                description: None,
                input_schema: None,
            },
        ];

        let schemas = list_tools_for_llm(&rt, Some(&catalog), None, None, &allowed);
        assert_eq!(schemas.len(), 1, "only the catalog-backed ref is emitted");
        let s = &schemas[0];
        assert_eq!(s.extension_id, "mcp:s1");
        assert_eq!(s.tool_name, "get_issue");
        assert_eq!(s.description, "Get an issue");
        assert_eq!(s.parameters, params);
    }

    #[test]
    fn non_mcp_ref_unchanged() {
        // A normal ref still routes through ext_runtime. The for_test runtime
        // loads no extensions, so list_tools errors → the ref is skipped,
        // exactly as in `list_tools_for_llm_with_no_extensions_returns_empty`.
        // The presence of a catalog must not change that path.
        //
        // The catalog deliberately contains an entry keyed by the FULL
        // non-mcp extension id — if the mcp branch ever matched non-`mcp:`
        // ids and consulted the catalog, this entry would be emitted and the
        // empty assertion below would catch the regression.
        let rt = ExtensionRuntime::for_test();
        let catalog = catalog_with(
            "greentic.tavily",
            "search",
            "decoy: must never be emitted for a non-mcp ref",
            serde_json::json!({}),
            None,
        );
        let allowed = vec![ToolRef {
            extension_id: "greentic.tavily".into(),
            tool_name: "search".into(),
            description: None,
            input_schema: None,
        }];
        let schemas = list_tools_for_llm(&rt, Some(&catalog), None, None, &allowed);
        assert!(
            schemas.is_empty(),
            "non-mcp ref still goes through ext_runtime (unloaded → dropped)"
        );
    }

    #[test]
    fn is_tool_allowed_matches_mcp_ref() {
        // Exact (mcp:s1, get_issue) match works with no change to the fn.
        let allowed = vec![ToolRef {
            extension_id: "mcp:s1".into(),
            tool_name: "get_issue".into(),
            description: None,
            input_schema: None,
        }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "mcp:s1".into(),
            tool_name: "get_issue".into(),
            args: serde_json::json!({}),
        };
        assert!(is_tool_allowed(&call, &allowed));

        let other = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "mcp:s1".into(),
            tool_name: "search_code".into(),
            args: serde_json::json!({}),
        };
        assert!(!is_tool_allowed(&other, &allowed));
    }

    #[tokio::test]
    async fn dispatch_routes_mcp_ref() {
        // Route present → calls the fake MCP server and returns its output.
        let mcp = fake_mcp_call_server(serde_json::json!({
            "structuredContent": { "ok": 1 }
        }))
        .await;
        let uri = mcp.uri();
        let catalog = Arc::new(catalog_with(
            "s1",
            "get_issue",
            "Get an issue",
            serde_json::json!({}),
            Some(&uri),
        ));
        let rt = Arc::new(ExtensionRuntime::for_test());

        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "mcp:s1".into(),
            tool_name: "get_issue".into(),
            args: serde_json::json!({}),
        };
        let tc = TenantContext::new("t", "e");
        let out = dispatch_tool_call(rt.clone(), Some(catalog.clone()), None, None, call, &tc)
            .await
            .expect("mcp dispatch never returns Err");
        assert_eq!(out, serde_json::json!({ "ok": 1 }), "got: {out}");

        // Route missing → shaped error value, still Ok.
        let missing = ToolCallRecord {
            call_id: "c2".into(),
            extension_id: "mcp:s1".into(),
            tool_name: "no_such".into(),
            args: serde_json::json!({}),
        };
        let out = dispatch_tool_call(rt.clone(), Some(catalog), None, None, missing, &tc)
            .await
            .expect("missing mcp route still returns Ok");
        assert_eq!(
            out,
            serde_json::json!({ "error": "unknown mcp tool 's1/no_such'" }),
            "got: {out}"
        );

        // A non-mcp call still hits the ext_runtime path (unloaded → Err).
        let non_mcp = ToolCallRecord {
            call_id: "c3".into(),
            extension_id: "greentic.absent".into(),
            tool_name: "nope".into(),
            args: serde_json::json!({}),
        };
        let res = dispatch_tool_call(rt, None, None, None, non_mcp, &tc).await;
        assert!(
            res.is_err(),
            "non-mcp dispatch against an unloaded extension must error"
        );
    }

    use crate::component_source::ComponentToolCatalog;
    use crate::component_source::test_support::{FakeInvoker, one_tool};
    use crate::flow_source::{FlowInvoker, FlowOperation, FlowToolCatalog};

    struct FakeFlowInvoker;
    impl FlowInvoker for FakeFlowInvoker {
        fn list_flows(&self) -> Vec<FlowOperation> {
            vec![FlowOperation {
                flow_ref: "lookup".into(),
                description: "Look things up".into(),
                parameters: serde_json::json!({ "type": "object", "properties": { "q": { "type": "integer" } } }),
            }]
        }
        fn invoke<'a>(
            &'a self,
            flow_ref: &'a str,
            args_json: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
        > {
            Box::pin(async move {
                if flow_ref == "lookup" {
                    Ok(serde_json::json!({ "echoed": args_json }))
                } else {
                    Err(format!("flow '{flow_ref}' not found"))
                }
            })
        }
    }

    fn ext_runtime_stub() -> ExtensionRuntime {
        ExtensionRuntime::for_test()
    }

    fn test_flow_invoker() -> FakeFlowInvoker {
        FakeFlowInvoker
    }

    #[test]
    fn flow_tool_prefers_author_contract_over_catalog() {
        // Catalog has flow "lookup" with its own description + parameters.
        // ToolRef carries author overrides — these must win.
        let flows = Arc::new(FlowToolCatalog::from_invoker(Arc::new(test_flow_invoker())));
        let allowed = vec![ToolRef {
            extension_id: "flow:lookup".into(),
            tool_name: "look_up".into(),
            description: Some("Author description".into()),
            input_schema: Some(
                serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}}),
            ),
        }];
        let schemas = list_tools_for_llm(&ext_runtime_stub(), None, None, Some(&flows), &allowed);
        let s = schemas
            .iter()
            .find(|s| s.extension_id == "flow:lookup")
            .expect("flow tool listed");
        assert_eq!(
            s.description, "Author description",
            "override description must win over catalog"
        );
        assert_eq!(
            s.parameters["properties"]["q"]["type"], "string",
            "override schema must win over catalog"
        );
    }

    #[test]
    fn flow_tool_falls_back_to_catalog_when_no_override() {
        // ToolRef has no override — the catalog entry must be used.
        let flows = Arc::new(FlowToolCatalog::from_invoker(Arc::new(test_flow_invoker())));
        let allowed = vec![ToolRef {
            extension_id: "flow:lookup".into(),
            tool_name: "look_up".into(),
            description: None,
            input_schema: None,
        }];
        let schemas = list_tools_for_llm(&ext_runtime_stub(), None, None, Some(&flows), &allowed);
        assert!(
            schemas.iter().any(|s| s.extension_id == "flow:lookup"),
            "with no override the catalog entry must still be used to list the tool"
        );
    }

    #[tokio::test]
    async fn flow_prefixed_tool_is_listed_and_dispatched() {
        let flows = Arc::new(FlowToolCatalog::from_invoker(Arc::new(FakeFlowInvoker)));
        let rt = ExtensionRuntime::for_test();
        let allowed = vec![ToolRef {
            extension_id: "flow:lookup".into(),
            tool_name: "look_up".into(),
            description: None,
            input_schema: None,
        }];
        let schemas = list_tools_for_llm(&rt, None, None, Some(&flows), &allowed);
        assert!(
            schemas
                .iter()
                .any(|s| s.extension_id == "flow:lookup" && s.tool_name == "look_up"),
            "flow: tool must appear in listed schemas"
        );

        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "flow:lookup".into(),
            tool_name: "look_up".into(),
            args: serde_json::json!({ "q": 1 }),
        };
        let rt_arc = Arc::new(ExtensionRuntime::for_test());
        let tc = TenantContext::new("t", "e");
        let out = dispatch_tool_call(rt_arc, None, None, Some(flows), call, &tc)
            .await
            .expect("flow dispatch must not return Err");
        assert!(
            out.get("error").is_none(),
            "known flow must dispatch, got {out}"
        );
    }

    #[test]
    fn component_ref_listed_from_catalog() {
        // A catalog-backed component: ref is emitted as an LlmToolSchema with
        // the catalog's description/parameters; ext_runtime is never consulted.
        let rt = ExtensionRuntime::for_test();
        let params = serde_json::json!({
            "type": "object",
            "properties": { "order_id": { "type": "string" } }
        });
        let invoker = Arc::new(FakeInvoker::new(vec![], Ok(serde_json::json!({}))));
        let catalog = ComponentToolCatalog::for_tests(
            one_tool(
                "greentic.refund",
                "issue_refund",
                "Issue a refund",
                params.clone(),
            ),
            invoker,
        );

        let allowed = vec![
            ToolRef {
                extension_id: "component:greentic.refund".into(),
                tool_name: "issue_refund".into(),
                description: None,
                input_schema: None,
            },
            // Absent from the catalog → dropped (warn), not panicked.
            ToolRef {
                extension_id: "component:greentic.refund".into(),
                tool_name: "missing".into(),
                description: None,
                input_schema: None,
            },
        ];

        let schemas = list_tools_for_llm(&rt, None, Some(&catalog), None, &allowed);
        assert_eq!(schemas.len(), 1, "only the catalog-backed ref is emitted");
        let s = &schemas[0];
        assert_eq!(s.extension_id, "component:greentic.refund");
        assert_eq!(s.tool_name, "issue_refund");
        assert_eq!(s.description, "Issue a refund");
        assert_eq!(s.parameters, params);
    }

    #[test]
    fn non_component_ref_unaffected_by_catalog() {
        // A plain ext ref still routes through ext_runtime even when a
        // component catalog is present. The decoy entry is keyed by the FULL
        // non-prefixed id — if the component branch ever matched it, this entry
        // would leak into the list and the empty assertion would catch it.
        let rt = ExtensionRuntime::for_test();
        let invoker = Arc::new(FakeInvoker::new(vec![], Ok(serde_json::json!({}))));
        let catalog = ComponentToolCatalog::for_tests(
            one_tool(
                "greentic.tavily",
                "search",
                "decoy: must never be emitted for a non-component ref",
                serde_json::json!({}),
            ),
            invoker,
        );
        let allowed = vec![ToolRef {
            extension_id: "greentic.tavily".into(),
            tool_name: "search".into(),
            description: None,
            input_schema: None,
        }];
        let schemas = list_tools_for_llm(&rt, None, Some(&catalog), None, &allowed);
        assert!(
            schemas.is_empty(),
            "non-component ref still goes through ext_runtime (unloaded → dropped)"
        );
    }

    #[tokio::test]
    async fn dispatch_routes_component_ref() {
        // Catalog entry present → routes to the invoker and returns its value.
        let invoker = Arc::new(FakeInvoker::new(
            vec![],
            Ok(serde_json::json!({ "refund_id": "r-1" })),
        ));
        let catalog = Arc::new(ComponentToolCatalog::for_tests(
            one_tool(
                "greentic.refund",
                "issue_refund",
                "Issue a refund",
                serde_json::json!({}),
            ),
            invoker,
        ));
        let rt = Arc::new(ExtensionRuntime::for_test());

        let tc = TenantContext::new("t", "e");
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "component:greentic.refund".into(),
            tool_name: "issue_refund".into(),
            args: serde_json::json!({}),
        };
        let out = dispatch_tool_call(rt.clone(), None, Some(catalog.clone()), None, call, &tc)
            .await
            .expect("component dispatch never returns Err");
        assert_eq!(out, serde_json::json!({ "refund_id": "r-1" }), "got: {out}");

        // Unknown op → shaped error value, still Ok.
        let missing = ToolCallRecord {
            call_id: "c2".into(),
            extension_id: "component:greentic.refund".into(),
            tool_name: "no_such".into(),
            args: serde_json::json!({}),
        };
        let out = dispatch_tool_call(rt.clone(), None, Some(catalog), None, missing, &tc)
            .await
            .expect("missing component op still returns Ok");
        assert!(out.to_string().contains("error"), "got: {out}");

        // No component catalog wired → shaped error value, still Ok (mirrors
        // the mcp branch's "no route" behaviour).
        let no_cat = ToolCallRecord {
            call_id: "c3".into(),
            extension_id: "component:greentic.refund".into(),
            tool_name: "issue_refund".into(),
            args: serde_json::json!({}),
        };
        let out = dispatch_tool_call(rt, None, None, None, no_cat, &tc)
            .await
            .expect("component dispatch with no catalog still returns Ok");
        assert!(out.to_string().contains("error"), "got: {out}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod kv_ledger_tests {
    use super::*;
    use crate::kv::MemoryKv;
    use std::sync::Arc;

    #[tokio::test]
    async fn record_then_get_replays_result() {
        let ledger = KvToolLedger::new(Arc::new(MemoryKv::new()));
        let t = TenantContext::new("acme", "prod");
        assert!(ledger.get(&t, "sess", "call1").await.unwrap().is_none());
        ledger
            .record(&t, "sess", "call1", serde_json::json!({"ok": true}))
            .await
            .unwrap();
        let got = ledger.get(&t, "sess", "call1").await.unwrap();
        assert_eq!(got, Some(serde_json::json!({"ok": true})));
    }
}
