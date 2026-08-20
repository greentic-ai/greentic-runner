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
pub mod aw {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use greentic_aw_runtime::{MCP_ROLE_FLOW_EDITOR, McpToolSource, TenantContext, dispatch_route};

    use crate::runner::mcp_pack_routes::{PackMcpRoute, PackMcpRoutes};

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

    /// Build a dispatchable route from a pack-carried record, resolving the
    /// credential from the secrets backend.
    ///
    /// The token is NEVER carried by the pack — it is read from
    /// `secrets://default/<tenant>/<team>/mcp/<server_id>`, the URI
    /// greentic-designer-admin writes under the `mcp` category, built by
    /// [`greentic_aw_runtime::mcp_secrets::mcp_secret_uri`]. That module is the
    /// single builder for the shape; do not re-derive it here.
    ///
    /// Only an `http` route reads a token. A `local-wasm` route has no HTTP
    /// credential — admin writes no `mcp/<server_id>` entry for one — so
    /// reading unconditionally would fail every local-wasm route on a
    /// credential that is not supposed to exist.
    async fn route_from_pack(
        route: &PackMcpRoute,
        secrets: Option<&crate::secrets::DynSecretsManager>,
        tenant: &str,
        team: Option<&str>,
    ) -> Result<greentic_aw_runtime::McpRoute, String> {
        let is_http = route.transport != "local-wasm";

        let token = match secrets.filter(|_| is_http) {
            Some(manager) => match greentic_aw_runtime::mcp_secrets::read_mcp_secret(
                manager.as_ref(),
                tenant,
                team,
                &route.server_id,
            )
            .await
            {
                Ok(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
                Err(miss) => return Err(format!("mcp server '{}' has {miss}", route.server_id)),
            },
            None => None,
        };

        Ok(greentic_aw_runtime::McpRoute::from_parts(
            &route.server_id,
            route.transport_url.as_deref().unwrap_or_default(),
            route.auth_header_name.as_deref(),
            token.as_deref(),
            &route.transport,
            route.component_ref.as_deref(),
            route.component_version.as_deref(),
            route.component_digest.as_deref(),
        ))
    }

    /// Invoke `tool` on `server_id` with `arguments`, preferring a
    /// pack-carried route and falling back to the flow-editor MCP catalog.
    ///
    /// Infallible by contract: every failure path returns a structured
    /// `{"error": ...}` value that the caller binds as-is.
    ///
    /// `secrets` is the HOST's manager, passed in rather than derived from
    /// `SECRETS_BACKEND`. That env names only `env` and `broker`, while an
    /// operator booting a bundle runs on greentic-start's dev store — so a
    /// manager built here could never read a pack route's credential, and the
    /// node would dispatch without one.
    #[allow(clippy::too_many_arguments)]
    pub async fn invoke_with_secrets(
        source: Option<&Arc<McpToolSource>>,
        pack_routes: Option<&PackMcpRoutes>,
        secrets: Option<&crate::secrets::DynSecretsManager>,
        tenant: &str,
        env: &str,
        team: Option<&str>,
        server_id: &str,
        tool: &str,
        arguments: &Value,
    ) -> Value {
        let args_str = arguments.to_string();

        // A pack-carried route wins. This is what lets a deployed runner with
        // no admin credentials dispatch at all; falling through to the admin
        // catalog leaves every existing deployment and Run Demo unchanged.
        if let Some(route) = pack_routes.and_then(|routes| routes.get(server_id)) {
            return match route_from_pack(route, secrets, tenant, team).await {
                Ok(resolved) => {
                    let result = dispatch_route(&resolved.with_tool(tool), &args_str).await;
                    if let Some(error) = result.get("error") {
                        tracing::warn!(
                            tenant,
                            env,
                            server_id,
                            tool,
                            error = %error,
                            "mcp node dispatch failed (pack-carried route)"
                        );
                    }
                    result
                }
                Err(e) => {
                    tracing::warn!(
                        tenant,
                        env,
                        server_id,
                        tool,
                        error = %e,
                        "mcp node did not run: pack-carried route could not be resolved"
                    );
                    json!({ "error": e })
                }
            };
        }

        // Every failure below is returned as a value, not an error, and the
        // node still reports `status=ok` — so without a log an operator sees a
        // clean run whose MCP call silently did nothing. Warn on each path.
        let Some(source) = source else {
            tracing::warn!(
                tenant,
                env,
                server_id,
                tool,
                "mcp node did not run: no route in the pack, and MCP is not configured \
                 on this runner (GREENTIC_AW_ADMIN_ENDPOINT + GREENTIC_AW_ADMIN_TOKEN)"
            );
            return json!({
                "error": "MCP is not configured on this runner (no route in the pack, and no \
                          GREENTIC_AW_ADMIN_ENDPOINT + GREENTIC_AW_ADMIN_TOKEN)"
            });
        };

        let tenant_ctx = TenantContext::new(tenant, env);
        let catalog = source
            .catalog_for_role(&tenant_ctx, MCP_ROLE_FLOW_EDITOR)
            .await;

        let Some(route) = catalog.route(server_id, tool) else {
            tracing::warn!(
                tenant,
                env,
                server_id,
                tool,
                "mcp node did not run: tool is absent from the tenant's flow_editor catalog"
            );
            return json!({
                "error": format!(
                    "mcp tool '{server_id}/{tool}' not found in the tenant's flow_editor catalog"
                )
            });
        };

        // `dispatch_route` takes the arguments as a JSON string and is itself
        // infallible (bad args / connect / timeout all become `{"error": ...}`).
        let result = dispatch_route(route, &args_str).await;
        // `dispatch_route` is infallible too: bad args, connect failures and
        // timeouts all come back as `{"error": ...}`. Surface those as well,
        // otherwise a dead MCP endpoint is indistinguishable from a good call.
        if let Some(error) = result.get("error") {
            tracing::warn!(
                tenant,
                env,
                server_id,
                tool,
                error = %error,
                "mcp node dispatch failed"
            );
        }
        result
    }
}

#[cfg(feature = "agentic-worker")]
pub(crate) use aw::{invoke_with_secrets, source_from_env};

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

#[cfg(all(test, feature = "agentic-worker"))]
mod failure_logging_tests {
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    /// Collects subscriber output so a test can assert on what was logged.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().expect("capture lock")).into_owned()
        }
    }

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// An MCP node on a runner without MCP credentials must say so in the log,
    /// not only in the value it binds.
    ///
    /// The bound `{"error": ...}` is invisible to an operator: the node still
    /// reports `status=ok`, so a flow whose MCP call did nothing looks like a
    /// clean run. Until the node can report failure properly, a WARN is the
    /// only signal that something went wrong.
    #[tokio::test]
    async fn an_unconfigured_mcp_node_warns_rather_than_failing_silently() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::WARN)
            .finish();

        let bound = tracing::subscriber::with_default(subscriber, || {
            futures::executor::block_on(super::aw::invoke_with_secrets(
                None,
                // no pack sidecar either: this is the "nothing configured
                // anywhere" path, which must still warn rather than fail mute.
                None,
                // and no host secrets manager.
                None,
                "acme",
                "prod",
                None,
                "srv-1",
                "create_quote",
                &json!({ "company": "Acme" }),
            ))
        });

        // The contract is unchanged: the caller still gets a structured error.
        assert!(
            bound.get("error").is_some(),
            "the bound value must still carry the error, got {bound}"
        );

        let logged = captured.text();
        assert!(
            logged.contains("WARN"),
            "an unconfigured MCP node must log at WARN; captured: {logged:?}"
        );
        assert!(
            logged.contains("create_quote"),
            "the warning must name the tool that did not run; captured: {logged:?}"
        );
    }
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
