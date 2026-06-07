//! Per-tenant agentic-worker MCP tool catalog.
//!
//! Builds a per-tenant snapshot of MCP tools by pulling the tenant's enabled
//! MCP servers from the admin (`/api/v1/designer/tenant/me/mcp-servers`),
//! keeping only the servers that carry the `agentic_worker` role, then probing
//! each (connect → `initialize` → `tools/list`) and recording per-tool schemas
//! plus the dispatch route. The snapshot is cached per tenant behind a short
//! TTL so a stable admin config avoids re-hitting the remote servers on every
//! step.
//!
//! Resilience contract (MCP must never break an agent step):
//! - A network or non-200 admin response degrades to an EMPTY (cached) catalog
//!   with a `warn` — [`McpToolSource::catalog`] is infallible by contract and
//!   never returns or propagates an error.
//! - A server that times out or errors during the probe is skipped with a
//!   `warn`; the catalog still returns with the servers that worked.
//! - [`dispatch_route`] always returns a JSON [`serde_json::Value`] and never
//!   panics — on any failure it returns `{"error": "..."}`.
//!
//! Unlike the designer (which namespaces tools as `mcp__server__tool`
//! strings), the runner keys every tool by a `(server_id, raw_tool_name)`
//! tuple — no string mangling. Task 2 consumes [`McpToolEntry`] to build an
//! `LlmToolSchema`, so each entry stores the raw `description` + JSON-Schema
//! `parameters`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::json;

use greentic_mcp_client::{McpAuth, McpClientOptions, McpHttpClient, McpToolDef};

use crate::tenant::TenantContext;

/// How long a built catalog is reused before a refetch is considered.
const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);

/// Per-server budget for the full connect → initialize → list/call sequence.
const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Role a server must carry to be exposed to agentic workers.
const MCP_ROLE_AGENTIC_WORKER: &str = "agentic_worker";

/// Wire row from `/api/v1/designer/tenant/me/mcp-servers`.
#[derive(Deserialize)]
struct WireRow {
    id: String,
    #[allow(dead_code)]
    name: String,
    transport_url: String,
    auth_header_name: Option<String>,
    auth_token: Option<String>,
    /// `None` = expose every tool; `Some([])` = none; else whitelist of raw names.
    allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Deserialize)]
struct WireBody {
    servers: Vec<WireRow>,
}

/// One enabled MCP server, with its token sealed in a [`SecretString`].
struct ParsedServer {
    id: String,
    transport_url: String,
    auth_header_name: Option<String>,
    auth_token: Option<SecretString>,
    allowed_tools: Option<Vec<String>>,
    roles: Vec<String>,
}

/// LLM-facing schema for one MCP tool: enough for Task 2 to build an
/// `LlmToolSchema`.
#[derive(Clone, Debug)]
pub struct McpToolEntry {
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Everything needed to re-connect and invoke one MCP tool.
///
/// The `auth_token` is redacted from [`Debug`] (hand-written below) and must
/// never be logged or serialized.
#[derive(Clone)]
pub struct McpRoute {
    server_id: String,
    transport_url: String,
    auth_header_name: Option<String>,
    auth_token: Option<SecretString>,
    raw_tool_name: String,
}

impl std::fmt::Debug for McpRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRoute")
            .field("server_id", &self.server_id)
            .field("transport_url", &self.transport_url)
            .field("auth_header_name", &self.auth_header_name)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("raw_tool_name", &self.raw_tool_name)
            .finish()
    }
}

/// Immutable per-tenant view of the agentic-worker MCP tool surface.
pub struct McpToolCatalog {
    /// `(server_id, raw_tool_name)` → LLM-facing tool schema.
    tools: HashMap<(String, String), McpToolEntry>,
    /// `(server_id, raw_tool_name)` → dispatch route.
    routes: HashMap<(String, String), McpRoute>,
    fetched_at: Instant,
}

impl McpToolCatalog {
    fn empty() -> Self {
        Self {
            tools: HashMap::new(),
            routes: HashMap::new(),
            fetched_at: Instant::now(),
        }
    }

    /// Iterate every `(server_id, raw_tool_name)` tool key with its schema.
    /// Task 2 uses this to build the LLM tool list.
    pub fn tools(&self) -> impl Iterator<Item = (&(String, String), &McpToolEntry)> {
        self.tools.iter()
    }

    /// Number of tools in the catalog.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the catalog exposes no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// LLM-facing schema for one tool, if present.
    pub fn tool_entry(&self, server_id: &str, tool: &str) -> Option<&McpToolEntry> {
        self.tools.get(&(server_id.to_string(), tool.to_string()))
    }

    /// Dispatch route for one tool, if present.
    pub fn route(&self, server_id: &str, tool: &str) -> Option<&McpRoute> {
        self.routes.get(&(server_id.to_string(), tool.to_string()))
    }
}

/// Per-tenant, TTL-gated source of agentic-worker MCP tool catalogs.
///
/// Mirrors [`crate::http_provider::HttpConfigProvider`]: the admin origin and
/// a tenant `gtc_live_*` bearer token are captured at construction; the tenant
/// is implied by the token, with the [`TenantContext`] used only as the cache
/// key.
pub struct McpToolSource {
    base_url: String,
    token: String,
    client: reqwest::Client,
    cache: DashMap<String, Arc<McpToolCatalog>>,
}

impl McpToolSource {
    /// `base_url` is the admin origin (no trailing slash needed); `token` is a
    /// tenant `gtc_live_*` key.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        // A per-request timeout bounds a hung admin so a slow registry cannot
        // block the step indefinitely (mirrors `HttpConfigProvider::new`).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client,
            cache: DashMap::new(),
        }
    }

    /// Stable per-tenant cache key. `TenantContext` exposes no single opaque
    /// id, so the `(tenant_id, env_id)` pair is joined — the same pair
    /// `TenantContext::key_prefix` is built from.
    fn cache_key(tenant: &TenantContext) -> String {
        format!("{}:{}", tenant.tenant_id, tenant.env_id)
    }

    /// Return the tenant's MCP tool catalog, rebuilding when stale or absent.
    ///
    /// Infallible by contract: any admin network or non-200 response degrades
    /// to an empty (cached) catalog with a `warn`, and a dead/slow MCP server
    /// is skipped with a `warn` while the catalog still returns the servers
    /// that worked.
    pub async fn catalog(&self, tenant: &TenantContext) -> Arc<McpToolCatalog> {
        let key = Self::cache_key(tenant);

        if let Some(entry) = self.cache.get(&key) {
            let snap = entry.value();
            if snap.fetched_at.elapsed() < CATALOG_TTL {
                return snap.clone();
            }
        }

        let built = Arc::new(self.build_catalog(&key).await);
        self.cache.insert(key, built.clone());
        built
    }

    /// Fetch the admin rows and probe each agentic-worker server. Always
    /// returns a catalog (possibly empty); never errors.
    async fn build_catalog(&self, tenant_key: &str) -> McpToolCatalog {
        let servers = match self.fetch_servers().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    tenant = %tenant_key,
                    error = %e,
                    "mcp-servers fetch failed; serving empty MCP catalog"
                );
                return McpToolCatalog::empty();
            }
        };

        let mut catalog = McpToolCatalog::empty();
        for server in &servers {
            if !server.roles.iter().any(|r| r == MCP_ROLE_AGENTIC_WORKER) {
                continue;
            }
            self.probe_server(&mut catalog, server, tenant_key).await;
        }
        catalog
    }

    /// GET the tenant's MCP servers from the admin. Returns an error string on
    /// any network failure or non-200 status (the caller degrades to empty).
    async fn fetch_servers(&self) -> Result<Vec<ParsedServer>, String> {
        let url = format!("{}/api/v1/designer/tenant/me/mcp-servers", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;

        let status = resp.status();
        if status.as_u16() != 200 {
            return Err(format!("admin returned status {}", status.as_u16()));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("decode failed: {e}"))?;
        parse_rows(body)
    }

    /// Probe one server and ingest its (filtered) tools into `catalog`. A
    /// timeout, connection error, or bad transport URL is skipped with a
    /// `warn` — the catalog keeps the servers that worked.
    async fn probe_server(
        &self,
        catalog: &mut McpToolCatalog,
        server: &ParsedServer,
        tenant_key: &str,
    ) {
        match tokio::time::timeout(SERVER_TIMEOUT, list_server_tools(server)).await {
            Ok(Ok(defs)) => ingest_server_tools(catalog, server, &defs),
            Ok(Err(e)) => tracing::warn!(
                tenant = %tenant_key,
                server = %server.id,
                error = %e,
                "mcp server probe failed; skipping its tools"
            ),
            Err(_) => tracing::warn!(
                tenant = %tenant_key,
                server = %server.id,
                "mcp server probe timed out after {}s; skipping its tools",
                SERVER_TIMEOUT.as_secs()
            ),
        }
    }
}

/// Parse the `{servers:[...]}` wire body into [`ParsedServer`]s.
fn parse_rows(body: serde_json::Value) -> Result<Vec<ParsedServer>, String> {
    let wire: WireBody = serde_json::from_value(body).map_err(|e| e.to_string())?;
    Ok(wire
        .servers
        .into_iter()
        .map(|w| ParsedServer {
            id: w.id,
            transport_url: w.transport_url,
            auth_header_name: w.auth_header_name,
            auth_token: w.auth_token.map(SecretString::from),
            allowed_tools: w.allowed_tools,
            roles: w.roles,
        })
        .collect())
}

/// Apply the server's `allowed_tools` filter and insert each accepted tool's
/// schema + route, keyed `(server_id, raw_tool_name)`.
fn ingest_server_tools(catalog: &mut McpToolCatalog, server: &ParsedServer, defs: &[McpToolDef]) {
    for def in defs {
        if let Some(allow) = &server.allowed_tools
            && !allow.iter().any(|a| a == &def.name)
        {
            continue;
        }
        let key = (server.id.clone(), def.name.clone());
        catalog.tools.insert(
            key.clone(),
            McpToolEntry {
                description: def.description.clone(),
                parameters: def.input_schema.clone(),
            },
        );
        catalog.routes.insert(
            key,
            McpRoute {
                server_id: server.id.clone(),
                transport_url: server.transport_url.clone(),
                auth_header_name: server.auth_header_name.clone(),
                auth_token: server.auth_token.clone(),
                raw_tool_name: def.name.clone(),
            },
        );
    }
}

/// Build an [`McpAuth`] from an optional token + optional header name.
fn build_auth(token: Option<&SecretString>, header_name: Option<&str>) -> Option<McpAuth> {
    token.map(|t| McpAuth {
        header_name: header_name.map(str::to_string),
        token: t.expose_secret().to_string(),
    })
}

fn client_opts() -> McpClientOptions {
    McpClientOptions {
        timeout: SERVER_TIMEOUT,
        client_name: "greentic-aw-runtime".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Construct a client for a parsed server row. Synchronous: `McpHttpClient::new`
/// does no I/O (the network handshake happens in `initialize`/`list_tools`).
fn connect(server: &ParsedServer) -> Result<McpHttpClient, String> {
    let endpoint = url::Url::parse(&server.transport_url)
        .map_err(|e| format!("invalid transport_url '{}': {e}", server.transport_url))?;
    let auth = build_auth(
        server.auth_token.as_ref(),
        server.auth_header_name.as_deref(),
    );
    McpHttpClient::new(endpoint, auth, client_opts()).map_err(|e| e.to_string())
}

/// Construct a client from a dispatch route.
fn connect_route(route: &McpRoute) -> Result<McpHttpClient, String> {
    let endpoint = url::Url::parse(&route.transport_url)
        .map_err(|e| format!("invalid transport_url '{}': {e}", route.transport_url))?;
    let auth = build_auth(route.auth_token.as_ref(), route.auth_header_name.as_deref());
    McpHttpClient::new(endpoint, auth, client_opts()).map_err(|e| e.to_string())
}

/// Connect, handshake, and list a server's tools. Errors are stringified.
async fn list_server_tools(server: &ParsedServer) -> Result<Vec<McpToolDef>, String> {
    let mut client = connect(server)?;
    client.initialize().await.map_err(|e| e.to_string())?;
    client.list_tools().await.map_err(|e| e.to_string())
}

/// Invoke an MCP tool through its route. Always returns a JSON [`Value`],
/// never panics — bad arguments, connection failures, and timeouts all become
/// `{"error": "..."}`.
pub async fn dispatch_route(route: &McpRoute, args: &str) -> serde_json::Value {
    let parsed: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(e) => return json!({ "error": format!("invalid tool arguments: {e}") }),
    };

    match tokio::time::timeout(SERVER_TIMEOUT, call_route(route, &parsed)).await {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => json!({ "error": e }),
        Err(_) => json!({
            "error": format!("tool call timed out after {}s", SERVER_TIMEOUT.as_secs())
        }),
    }
}

async fn call_route(
    route: &McpRoute,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut client = connect_route(route)?;
    client.initialize().await.map_err(|e| e.to_string())?;
    let out = client
        .call_tool(&route.raw_tool_name, args)
        .await
        .map_err(|e| e.to_string())?;
    Ok(out.to_value())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    /// Mount the 4-call MCP JSON-RPC contract (initialize, initialized,
    /// tools/list, tools/call) on a fresh wiremock server.
    async fn fake_mcp_server(
        tools_json: serde_json::Value,
        call_result_json: serde_json::Value,
    ) -> MockServer {
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
                "result": { "tools": tools_json }
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

    fn two_tools() -> serde_json::Value {
        json!([
            { "name": "get_issue", "description": "Get an issue",
              "inputSchema": { "type": "object", "properties": { "id": { "type": "string" } } } },
            { "name": "search_code", "description": "Search code",
              "inputSchema": { "type": "object" } }
        ])
    }

    /// Build a `{servers:[...]}` admin body for one agentic-worker server.
    fn admin_body_agentic(
        id: &str,
        url: &str,
        allowed: Option<Vec<&str>>,
        roles: Vec<&str>,
    ) -> serde_json::Value {
        json!({
            "servers": [
                {
                    "id": id,
                    "name": "Server",
                    "transport_url": url,
                    "auth_header_name": null,
                    "auth_token": null,
                    "allowed_tools": allowed.map(|v| v.into_iter().collect::<Vec<_>>()),
                    "roles": roles,
                }
            ]
        })
    }

    /// Mount the admin `mcp-servers` endpoint returning `body`, asserting the
    /// bearer token and path.
    async fn mount_admin(server: &MockServer, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path("/api/v1/designer/tenant/me/mcp-servers"))
            .and(header("authorization", "Bearer gtc_live_x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    fn tenant() -> TenantContext {
        TenantContext::new("acme", "prod")
    }

    #[tokio::test]
    async fn source_fetches_and_filters_agentic_worker() {
        // One agentic_worker server (→ fake MCP) and one flow_editor server.
        let mcp = fake_mcp_server(two_tools(), json!({})).await;
        let admin = MockServer::start().await;
        let body = json!({
            "servers": [
                {
                    "id": "worker", "name": "Worker", "transport_url": mcp.uri(),
                    "auth_header_name": null, "auth_token": null,
                    "allowed_tools": null, "roles": ["agentic_worker"]
                },
                {
                    "id": "editor", "name": "Editor", "transport_url": "http://127.0.0.1:1/",
                    "auth_header_name": null, "auth_token": null,
                    "allowed_tools": null, "roles": ["flow_editor"]
                }
            ]
        });
        mount_admin(&admin, body).await;

        let source = McpToolSource::new(admin.uri(), "gtc_live_x");
        let catalog = source.catalog(&tenant()).await;

        // Only the agentic_worker server's two tools land.
        assert_eq!(catalog.len(), 2);
        assert!(catalog.tool_entry("worker", "get_issue").is_some());
        assert!(catalog.tool_entry("worker", "search_code").is_some());
        // The flow_editor server is never probed/ingested.
        assert!(catalog.tool_entry("editor", "get_issue").is_none());
    }

    #[tokio::test]
    async fn catalog_applies_allowed_tools() {
        let mcp = fake_mcp_server(two_tools(), json!({})).await;
        let admin = MockServer::start().await;
        mount_admin(
            &admin,
            admin_body_agentic(
                "s1",
                &mcp.uri(),
                Some(vec!["get_issue"]),
                vec!["agentic_worker"],
            ),
        )
        .await;

        let source = McpToolSource::new(admin.uri(), "gtc_live_x");
        let catalog = source.catalog(&tenant()).await;

        assert_eq!(catalog.len(), 1);
        assert!(catalog.tool_entry("s1", "get_issue").is_some());
        assert!(catalog.tool_entry("s1", "search_code").is_none());
    }

    #[tokio::test]
    async fn unreachable_server_degrades() {
        let admin = MockServer::start().await;
        mount_admin(
            &admin,
            admin_body_agentic("s1", "http://127.0.0.1:1/", None, vec!["agentic_worker"]),
        )
        .await;

        let source = McpToolSource::new(admin.uri(), "gtc_live_x");
        // Must not panic and must return a catalog (with no tools for the
        // dead server).
        let catalog = source.catalog(&tenant()).await;
        assert_eq!(catalog.len(), 0);
        assert!(catalog.is_empty());
    }

    #[tokio::test]
    async fn admin_unreachable_degrades_to_empty() {
        let admin = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/designer/tenant/me/mcp-servers"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&admin)
            .await;

        let source = McpToolSource::new(admin.uri(), "gtc_live_x");
        let catalog = source.catalog(&tenant()).await;
        assert!(catalog.is_empty());
    }

    #[tokio::test]
    async fn ttl_cache_reuses_within_window() {
        let mcp = fake_mcp_server(two_tools(), json!({})).await;
        let admin = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/designer/tenant/me/mcp-servers"))
            .and(header("authorization", "Bearer gtc_live_x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(admin_body_agentic(
                "s1",
                &mcp.uri(),
                None,
                vec!["agentic_worker"],
            )))
            .expect(1) // second call must hit the TTL cache
            .mount(&admin)
            .await;

        let source = McpToolSource::new(admin.uri(), "gtc_live_x");
        let t = tenant();
        let first = source.catalog(&t).await;
        let second = source.catalog(&t).await;
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn dispatch_route_calls_server_and_wraps() {
        // Success: fake returns structuredContent.
        let mcp = fake_mcp_server(two_tools(), json!({ "structuredContent": { "ok": 1 } })).await;
        let admin = MockServer::start().await;
        mount_admin(
            &admin,
            admin_body_agentic(
                "s1",
                &mcp.uri(),
                Some(vec!["get_issue"]),
                vec!["agentic_worker"],
            ),
        )
        .await;

        let source = McpToolSource::new(admin.uri(), "gtc_live_x");
        let catalog = source.catalog(&tenant()).await;
        let route = catalog.route("s1", "get_issue").expect("route present");

        let out = dispatch_route(route, "{}").await;
        // `ToolOutput::to_value` unwraps `structuredContent`, so the value is
        // the server's structured payload itself.
        assert_eq!(out, json!({ "ok": 1 }), "got: {out}");
        assert!(!out.to_string().contains("\"error\""), "got: {out}");

        // isError: fake returns an error envelope → Value contains "error".
        let mcp_err = fake_mcp_server(
            two_tools(),
            json!({ "isError": true, "content": [{ "type": "text", "text": "boom" }] }),
        )
        .await;
        let admin_err = MockServer::start().await;
        mount_admin(
            &admin_err,
            admin_body_agentic(
                "s1",
                &mcp_err.uri(),
                Some(vec!["get_issue"]),
                vec!["agentic_worker"],
            ),
        )
        .await;
        let source_err = McpToolSource::new(admin_err.uri(), "gtc_live_x");
        let catalog_err = source_err.catalog(&TenantContext::new("acme", "stg")).await;
        let route_err = catalog_err.route("s1", "get_issue").expect("route present");
        let out_err = dispatch_route(route_err, "{}").await;
        assert!(out_err.to_string().contains("error"), "got: {out_err}");
    }

    #[tokio::test]
    async fn route_debug_redacts_token() {
        let route = McpRoute {
            server_id: "s1".to_string(),
            transport_url: "https://mcp.example.com/".to_string(),
            auth_header_name: None,
            auth_token: Some(SecretString::from("tok-secret".to_string())),
            raw_tool_name: "get_issue".to_string(),
        };
        let dbg = format!("{route:?}");
        assert!(!dbg.contains("tok-secret"), "got: {dbg}");
        assert!(dbg.contains("[REDACTED]"), "got: {dbg}");
    }
}
