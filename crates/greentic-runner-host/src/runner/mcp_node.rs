//! Flow-execution MCP node (LOCKED ENCODING v2: `component == "mcp"`).
//!
//! The node carries `server`, `tool`, `arguments` and an optional `output`
//! state key in its payload/config — the same shape `greentic-flow` lowers
//! designer MCP nodes to. The payload is the source of truth.
//!
//! This is the SEPARATE flow-execution MCP path (role `flow_editor`), distinct
//! from the agent-loop MCP path in `greentic-aw-runtime` (role
//! `agentic_worker`). It reuses [`McpToolSource`] — the same per-tenant
//! admin-fetch → role-filter → probe → TTL-cache → `dispatch_route` machinery
//! the agent loop uses — only swapping the role filter to `flow_editor`.
//!
//! Resilience contract (MCP must never break a flow run): if MCP is
//! unconfigured (no admin env / opt-out) or the tool is unreachable, the node
//! produces a structured error value, never a panic and never an aborted
//! runtime. The engine binds that value into flow state exactly like a
//! successful call, so downstream nodes can branch on the `error` key.

#[cfg(feature = "agentic-worker")]
mod aw {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use greentic_aw_runtime::{MCP_ROLE_FLOW_EDITOR, McpToolSource, TenantContext, dispatch_route};

    /// Build an [`McpToolSource`] for the flow-execution path from the same
    /// admin credentials the agentic-worker registry uses
    /// (`GREENTIC_AW_ADMIN_ENDPOINT` + `GREENTIC_AW_ADMIN_TOKEN`).
    ///
    /// Mirrors `agent_node::mcp_source_from_env`: MCP is ON by default whenever
    /// the admin credentials are present, with `GREENTIC_AW_MCP=0` as the
    /// operator opt-out. Returns `None` on opt-out or when either credential is
    /// missing/empty, so a runner without MCP configured simply fails MCP nodes
    /// gracefully rather than constructing a useless source.
    pub(crate) fn source_from_env() -> Option<Arc<McpToolSource>> {
        if std::env::var("GREENTIC_AW_MCP").ok().as_deref() == Some("0") {
            tracing::info!("GREENTIC_AW_MCP=0; flow MCP node source disabled");
            return None;
        }
        let endpoint = std::env::var("GREENTIC_AW_ADMIN_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())?;
        let token = std::env::var("GREENTIC_AW_ADMIN_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())?;
        tracing::info!(endpoint = %endpoint, "flow MCP node source constructed");
        Some(Arc::new(McpToolSource::new(endpoint, token)))
    }

    /// Invoke `tool` on `server_id` for `tenant`/`env` with `arguments`,
    /// reusing the flow-editor MCP catalog.
    ///
    /// Infallible by contract: every failure path (source not configured,
    /// server/tool not in the flow-editor catalog, transport error) returns a
    /// structured `{"error": "..."}` value. The caller binds the value as-is.
    pub(crate) async fn invoke(
        source: Option<&Arc<McpToolSource>>,
        tenant: &str,
        env: &str,
        server_id: &str,
        tool: &str,
        arguments: &Value,
    ) -> Value {
        let Some(source) = source else {
            return json!({
                "error": "MCP is not configured on this runner (set GREENTIC_AW_ADMIN_ENDPOINT + GREENTIC_AW_ADMIN_TOKEN)"
            });
        };

        let tenant_ctx = TenantContext::new(tenant, env);
        let catalog = source
            .catalog_for_role(&tenant_ctx, MCP_ROLE_FLOW_EDITOR)
            .await;

        let Some(route) = catalog.route(server_id, tool) else {
            return json!({
                "error": format!(
                    "mcp tool '{server_id}/{tool}' not found in the tenant's flow_editor catalog"
                )
            });
        };

        // `dispatch_route` takes the arguments as a JSON string and is itself
        // infallible (bad args / connect / timeout all become `{"error": ...}`).
        let args_str = arguments.to_string();
        dispatch_route(route, &args_str).await
    }
}

#[cfg(feature = "agentic-worker")]
pub(crate) use aw::{invoke, source_from_env};

use serde_json::Value;

/// Read a non-empty string field from a JSON object payload, trimming
/// surrounding whitespace. Returns `None` when the payload is not an object,
/// the key is absent, the value is not a string, or the trimmed value is empty.
pub(crate) fn str_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Extract the `(server, tool)` pair from an MCP node payload/config object
/// (LOCKED ENCODING v2). Both keys must be present and non-empty; otherwise the
/// caller falls back to the legacy `operation`/`mcp:` encoding.
pub(crate) fn server_tool_from_payload(payload: &Value) -> Option<(String, String)> {
    let server = str_field(payload, "server")?;
    let tool = str_field(payload, "tool")?;
    Some((server, tool))
}

#[cfg(test)]
mod tests {
    use super::{server_tool_from_payload, str_field};
    use serde_json::json;

    #[test]
    fn reads_server_and_tool_from_payload() {
        assert_eq!(
            server_tool_from_payload(&json!({ "server": "github", "tool": "get_issue" })),
            Some(("github".to_string(), "get_issue".to_string()))
        );
    }

    #[test]
    fn tolerates_dotted_tool_names_in_payload() {
        assert_eq!(
            server_tool_from_payload(&json!({ "server": "srv", "tool": "do.thing" })),
            Some(("srv".to_string(), "do.thing".to_string()))
        );
    }

    #[test]
    fn missing_or_empty_fields_yield_none() {
        assert_eq!(
            server_tool_from_payload(&json!({ "server": "github" })),
            None
        );
        assert_eq!(
            server_tool_from_payload(&json!({ "server": "", "tool": "get_issue" })),
            None
        );
        assert_eq!(
            server_tool_from_payload(&json!({ "server": "github", "tool": "   " })),
            None
        );
        assert_eq!(server_tool_from_payload(&json!("not an object")), None);
    }

    #[test]
    fn str_field_trims_and_rejects_empty() {
        assert_eq!(
            str_field(&json!({ "k": "  v  " }), "k"),
            Some("v".to_string())
        );
        assert_eq!(str_field(&json!({ "k": "" }), "k"), None);
        assert_eq!(str_field(&json!({ "k": 7 }), "k"), None);
        assert_eq!(str_field(&json!({}), "missing"), None);
    }
}
