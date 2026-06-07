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
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};

use crate::config::ToolRef;
use crate::error::{AgentError, StateError};
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
pub fn list_tools_for_llm(
    ext_runtime: &ExtensionRuntime,
    mcp: Option<&McpToolCatalog>,
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

/// Dispatch a single tool call. Wraps the blocking `invoke_tool` in
/// `tokio::task::spawn_blocking` so the async executor thread is never
/// stalled. Returns the tool result as a JSON Value.
///
/// Calls whose `extension_id` starts with `"mcp:"` route through the
/// per-tenant [`McpToolCatalog`] (`mcp`) instead: the suffix is the MCP
/// `server_id`, and dispatch goes over HTTP via
/// [`crate::mcp_source::dispatch_route`]. An MCP call NEVER yields `Err` — a
/// missing route or remote failure is surfaced as an `{"error": ...}` value so
/// the LLM observes it as a normal tool result. Non-mcp ids keep the existing
/// blocking WASM path.
pub async fn dispatch_tool_call(
    ext_runtime: Arc<ExtensionRuntime>,
    mcp: Option<Arc<McpToolCatalog>>,
    call: ToolCallRecord,
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
            None => serde_json::json!({
                "error": format!("unknown mcp tool '{}/{}'", server_id, call.tool_name)
            }),
        };
        return Ok(value);
    }

    let args_json = call.args.to_string();
    let extension_id = call.extension_id.clone();
    let tool_name = call.tool_name.clone();
    let raw = tokio::task::spawn_blocking(move || {
        ext_runtime.invoke_tool(&extension_id, &tool_name, &args_json)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn is_tool_allowed_returns_true_for_exact_match() {
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
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
        }];
        let schemas = list_tools_for_llm(&rt, None, &allowed);
        assert!(schemas.is_empty());
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
            },
            // Absent from the catalog → dropped (warn), not panicked.
            ToolRef {
                extension_id: "mcp:s1".into(),
                tool_name: "missing".into(),
            },
        ];

        let schemas = list_tools_for_llm(&rt, Some(&catalog), &allowed);
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
        let rt = ExtensionRuntime::for_test();
        let catalog = catalog_with(
            "s1",
            "get_issue",
            "Get an issue",
            serde_json::json!({}),
            None,
        );
        let allowed = vec![ToolRef {
            extension_id: "greentic.tavily".into(),
            tool_name: "search".into(),
        }];
        let schemas = list_tools_for_llm(&rt, Some(&catalog), &allowed);
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
        let out = dispatch_tool_call(rt.clone(), Some(catalog.clone()), call)
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
        let out = dispatch_tool_call(rt.clone(), Some(catalog), missing)
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
        let res = dispatch_tool_call(rt, None, non_mcp).await;
        assert!(
            res.is_err(),
            "non-mcp dispatch against an unloaded extension must error"
        );
    }
}
