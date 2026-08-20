//! Type definitions for the per-tenant agentic-worker MCP tool catalog.
//!
//! Contains the wire types, domain types, and the immutable catalog snapshot
//! ([`McpToolCatalog`]). Transport and dispatch logic lives in `source.rs`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use secrecy::SecretString;
use serde::Deserialize;

/// How long a built catalog is reused before a refetch is considered.
pub(super) const CATALOG_TTL: Duration = Duration::from_secs(5 * 60);

/// Per-server budget for the full connect → initialize → list/call sequence.
pub(super) const SERVER_TIMEOUT: Duration = Duration::from_secs(5);

/// Role a server must carry to be exposed to agentic workers (the agent-loop
/// MCP path).
pub const MCP_ROLE_AGENTIC_WORKER: &str = "agentic_worker";

/// Role a server must carry to be exposed to the flow-execution MCP path
/// (`mcp:<server>/<tool>` flow nodes). Distinct from
/// [`MCP_ROLE_AGENTIC_WORKER`] so the operator can authorize a server for one
/// surface without the other.
pub const MCP_ROLE_FLOW_EDITOR: &str = "flow_editor";

/// Which transport backs an MCP server row. `http` is the default (existing
/// remote behavior); `local-wasm` runs a `wasix:mcp` component in-process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    Http,
    LocalWasm,
}

/// Wire row from `/api/v1/designer/tenant/me/mcp-servers`.
#[derive(Deserialize)]
pub(super) struct WireRow {
    pub(super) id: String,
    #[allow(dead_code)]
    pub(super) name: String,
    /// `Option` because the admin serializes `transport_url: null` for
    /// `local-wasm` servers (they have no URL). A bare `String` here makes
    /// serde reject the WHOLE server list on the first local-wasm row, silently
    /// dropping every MCP tool for that tenant. `#[serde(default)]` also tolerates
    /// the field being absent. Mapped to `""` in `parse_rows`; the local-wasm
    /// branch never parses it as a URL.
    #[serde(default)]
    pub(super) transport_url: Option<String>,
    pub(super) auth_header_name: Option<String>,
    pub(super) auth_token: Option<String>,
    /// `None` = expose every tool; `Some([])` = none; else whitelist of raw names.
    pub(super) allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub(super) roles: Vec<String>,
    /// Transport discriminator; defaults to [`Transport::Http`] when absent so
    /// every existing row continues to work without changes.
    #[serde(default)]
    pub(super) transport: Transport,
    /// For `local-wasm` rows: the component reference (e.g. `weather.component`).
    pub(super) component_ref: Option<String>,
    /// For `local-wasm` rows: optional pinned component version.
    pub(super) component_version: Option<String>,
    /// For `local-wasm` rows: optional pinned component content digest (hex).
    #[serde(default)]
    pub(super) component_digest: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct WireBody {
    pub(super) servers: Vec<WireRow>,
}

/// One enabled MCP server, with its token sealed in a [`SecretString`].
pub(super) struct ParsedServer {
    pub(super) id: String,
    pub(super) transport_url: String,
    pub(super) auth_header_name: Option<String>,
    pub(super) auth_token: Option<SecretString>,
    pub(super) allowed_tools: Option<Vec<String>>,
    pub(super) roles: Vec<String>,
    pub(super) transport: Transport,
    pub(super) component_ref: Option<String>,
    pub(super) component_version: Option<String>,
    pub(super) component_digest: Option<String>,
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
    pub(super) server_id: String,
    pub(super) transport_url: String,
    pub(super) auth_header_name: Option<String>,
    pub(super) auth_token: Option<SecretString>,
    pub(super) raw_tool_name: String,
    pub transport: Transport,
    pub component_ref: Option<String>,
    pub component_version: Option<String>,
    pub component_digest: Option<String>,
}

impl McpRoute {
    /// Build a route from parts supplied by an embedding host (a pack-carried
    /// record), rather than from an admin row. The raw tool name is set per
    /// call by the dispatcher, so it starts empty.
    ///
    /// This exists because [`McpRoute`]'s fields are `pub(super)`: a host that
    /// resolves a route WITHOUT the admin catalog — a deployed runner reading
    /// `assets/mcp-routes.json` out of its own pack — otherwise has no way to
    /// express one.
    ///
    /// `auth_token` is sealed in a [`SecretString`] on the way in and stays
    /// redacted from [`Debug`], exactly as it is for an admin-built route.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        server_id: &str,
        transport_url: &str,
        auth_header_name: Option<&str>,
        auth_token: Option<&str>,
        transport: &str,
        component_ref: Option<&str>,
        component_version: Option<&str>,
        component_digest: Option<&str>,
    ) -> Self {
        Self {
            server_id: server_id.to_string(),
            transport_url: transport_url.to_string(),
            auth_header_name: auth_header_name.map(str::to_string),
            auth_token: auth_token.map(SecretString::from),
            raw_tool_name: String::new(),
            transport: if transport == "local-wasm" {
                Transport::LocalWasm
            } else {
                Transport::Http
            },
            component_ref: component_ref.map(str::to_string),
            component_version: component_version.map(str::to_string),
            component_digest: component_digest.map(str::to_string),
        }
    }

    /// Name the tool this route is about to call.
    #[must_use]
    pub fn with_tool(mut self, raw_tool_name: &str) -> Self {
        self.raw_tool_name = raw_tool_name.to_string();
        self
    }
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
            .field("transport", &self.transport)
            .field("component_ref", &self.component_ref)
            .field("component_version", &self.component_version)
            .field("component_digest", &self.component_digest)
            .finish()
    }
}

/// Immutable per-tenant view of the agentic-worker MCP tool surface.
pub struct McpToolCatalog {
    /// `(server_id, raw_tool_name)` → LLM-facing tool schema.
    pub(super) tools: HashMap<(String, String), McpToolEntry>,
    /// `(server_id, raw_tool_name)` → dispatch route.
    pub(super) routes: HashMap<(String, String), McpRoute>,
    pub(super) fetched_at: Instant,
}

impl McpToolCatalog {
    pub(super) fn empty() -> Self {
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

    /// Build a catalog directly from tool/route maps, bypassing the admin +
    /// MCP probe. Test-only: lets downstream crates (e.g. `tools.rs`) exercise
    /// the list/dispatch seams without standing up a wiremock pair.
    #[cfg(test)]
    pub(crate) fn for_tests(
        tools: HashMap<(String, String), McpToolEntry>,
        routes: HashMap<(String, String), McpRoute>,
    ) -> Self {
        Self {
            tools,
            routes,
            fetched_at: Instant::now(),
        }
    }
}

/// Build a dispatch route pointing at `transport_url` for `(server_id, tool)`.
/// Test-only constructor: `McpRoute` fields are private, so downstream test
/// code uses this to aim a route at a fake MCP server.
#[cfg(test)]
pub(crate) fn route_for_tests(server_id: &str, tool: &str, transport_url: &str) -> McpRoute {
    McpRoute {
        server_id: server_id.to_string(),
        transport_url: transport_url.to_string(),
        auth_header_name: None,
        auth_token: None,
        raw_tool_name: tool.to_string(),
        transport: Transport::Http,
        component_ref: None,
        component_version: None,
        component_digest: None,
    }
}
