# Agentic Worker MCP Tools — Design (MCP-4)

Date: 2026-06-07
Status: proposed, pending approval
Target repo: `greentic-runner` (greenticai), three-tier → branch from / PR to `research`
Depends on: admin endpoint `GET /api/v1/designer/tenant/me/mcp-servers` (live; **accepts the
runner's `gtc_live_*` tenant token** — verified, no admin change needed) and `greentic-mcp-client`.
Parent epic: `docs/mcp-in-designer-assessment-2026-06-06.md`. Sibling: MCP-3 (designer flow-editor,
designer PR #505) — this is the `agentic_worker`-role counterpart.

## Decisions locked (with Bima, 2026-06-07)
1. **Per-agent opt-in (allowlist)** — an agent only sees the MCP tools it explicitly references in
   its config, exactly like it already references extension tools. NOT auto-all.
2. **Full path to production** — aw-runtime plumbing + runner-host wiring + tests, targeting
   `research` in `greentic-runner` (works in both the designer playground via test-mock and the
   real runner — same `AgentRuntime` code path, only storage backends differ).

## Problem
Agentic workers today can only call WASM-extension tools: `AgentConfig.tools: Vec<ToolRef{extension_id,
tool_name}>` → `list_tools_for_llm`/`dispatch_tool_call` resolve every ref through
`ExtensionRuntime` (`crates/greentic-aw-runtime/src/tools.rs`). The admin MCP registry already stores
servers with an `agentic_worker` role, but **nothing consumes it** — MCP-4 is its first consumer.

## Architecture (mirror the existing `http_provider.rs` pattern)

The runner already fetches per-tenant agent config from designer-admin over HTTP with a `gtc_live_*`
bearer (`crates/greentic-aw-runtime/src/http_provider.rs`, layered via `LayeredConfigProvider` in
`runner-host/src/runner/agent_node.rs`). MCP-4 adds a parallel per-tenant MCP source and threads its
tools through the existing allowlist + dispatch seams.

```
admin /tenant/me/mcp-servers ──(reqwest GET, gtc_live_* bearer)──▶ McpToolSource
   (agentic_worker-filtered, per-tenant, TTL-cached)                 │
                                                                     ▼
AgentConfig.tools (allowlist, incl. mcp refs) ──┐         per-tenant catalog:
                                                ▼          server → [tool defs] + routes
   list_tools_for_llm(ext_runtime, mcp, &tools) ──▶ LLM sees only the agent's allowlisted
                                                │    MCP tools (schemas from the catalog)
   dispatch_tool_call(ext_runtime, mcp, call) ──▶ mcp ref → greentic-mcp-client call_tool
                                                   else    → ExtensionRuntime.invoke_tool
```

## Components

### 1. MCP tool source — `crates/greentic-aw-runtime/src/mcp_source.rs` (new)
Mirror `http_provider.rs` for the registry fetch + MCP-3's `McpCatalog` for the tool build:
- `McpToolSource { base_url: String, token: String, client: reqwest::Client, cache: <per-tenant TTL> }`,
  constructed from the SAME env the agent registry uses (`registry_from_env()` already yields base_url
  + `gtc_live_*` token — reuse it; no new config surface).
- `async fn catalog(&self, tenant: &TenantContext) -> Arc<McpToolCatalog>`: GET
  `{base}/api/v1/designer/tenant/me/mcp-servers` with `bearer_auth(token)`, filter rows to
  `roles.contains("agentic_worker")`, then for each server `greentic_mcp_client::McpHttpClient`
  `initialize` + `list_tools` (5s/server timeout), apply the server's own `allowed_tools`. Build:
  - `tools: HashMap<ToolKey, LlmToolSchema-ish>` keyed by `(server_id, raw_tool_name)`,
  - `routes: HashMap<ToolKey, McpRoute>` (server url + auth + raw name),
  TTL-cached (5 min) per tenant, **stale-on-error / degrade-to-empty** — a dead server or admin
  failure NEVER fails an agent turn (it just omits those tools; logged once per health transition,
  reusing MCP-3's throttle idea).
- The MCP server→server auth token (per the admin row's `auth_token`) stays in `SecretString`,
  never logged, never serialized.

This is a near-clone of designer's `src/ui/mcp/catalog.rs` adapted to aw-runtime's types; the
`greentic-mcp-client` crate (already reqwest 0.13) is the shared transport. We do NOT depend on
designer code (package boundary) — the catalog logic is small enough to port.

### 2. The opt-in reference convention
An agent allowlists an MCP tool with a reserved `extension_id` namespace, parallel to extension refs:
```json
{ "extension_id": "mcp:<server_id>", "tool_name": "<raw_tool_name>" }
```
- `extension_id` starting with `mcp:` is the discriminator (chosen over MCP-3's `mcp__` name-mangling
  because here the author writes the ref by hand/UI and `mcp:<server>` + real tool name is the most
  legible, least-surprising form; the runtime never needs a flattened single string).
- `<server_id>` is the admin MCP server id (stable); `<raw_tool_name>` is the server's own tool name.
- No change to the `ToolRef` struct — the existing `{extension_id, tool_name}` shape carries it. The
  admin agent-config JSON and the `{agent_id}.json` manifest both already accept arbitrary
  `extension_id` strings, so **no admin or manifest schema change** is required.

### 3. Dispatch + listing branch — `crates/greentic-aw-runtime/src/tools.rs`
Add an MCP branch at both seams, MCP refs resolved from the catalog, everything else unchanged:
- `list_tools_for_llm(ext_runtime, mcp: Option<&McpToolCatalog>, allowed: &[ToolRef]) -> Vec<LlmToolSchema>`:
  for a ref with `extension_id` starting `mcp:`, look up `(server_id, tool_name)` in the catalog and
  emit its schema (skip with a debug log if absent — server down/unknown); else the existing
  `ext_runtime.list_tools` path.
- `is_tool_allowed` already does exact `(extension_id, tool_name)` match — works unchanged for
  `mcp:<server>` refs (the LLM can only call what was offered).
- `dispatch_tool_call(ext_runtime, mcp, call)`: `mcp:`-prefixed → resolve route, call
  `greentic-mcp-client` `call_tool` (always returns a JSON value or an error value — never panics,
  same contract as the extension path); else the existing `ext_runtime.invoke_tool` path.

### 4. Wiring `McpToolSource` into the runtime
- `AgentRuntime::new(...)` gains an `mcp: Option<Arc<McpToolSource>>` arg (Option so test-mock and
  no-admin deployments pass `None` → zero MCP tools, never an error). The loop
  (`crates/greentic-aw-runtime/src/loop.rs`) resolves the per-tenant catalog once per turn (cheap —
  TTL cache) and passes it to `list_tools_for_llm` / `dispatch_tool_call`.
- `runner-host/src/runner/agent_node.rs`: build `Some(McpToolSource::new(base_url, token))` from the
  same `registry_from_env()` that already yields the agent-registry endpoint + token; `None` when the
  registry env is unset (today's default). Gated behind the existing env, plus an explicit
  `GREENTIC_AW_MCP=1` opt-in flag so enabling MCP tools is a deliberate operator action.

## Error / safety invariants (identical philosophy to MCP-3)
| Failure | Behavior |
|---|---|
| Admin registry unreachable / 401 / malformed | catalog degrades to empty; agent runs with only its extension tools; throttled warn |
| One MCP server down at catalog build | that server's tools omitted (or served stale within TTL); warn; agent continues |
| MCP server errors/timeouts at dispatch | tool result is an error value the LLM observes; turn continues; never panics |
| Agent references an MCP tool not in the catalog | tool simply not offered to the LLM; debug log |
| No admin / test-mock | `mcp = None` → zero MCP tools, zero overhead |
**An MCP failure never aborts or panics an agent turn.**

## Testing
- Unit (aw-runtime): catalog build (agentic_worker filter, allowed_tools, namespacing-by-id,
  stale/degrade), `mcp:`-ref listing + dispatch branch, `is_tool_allowed` for mcp refs.
- Integration (`crates/greentic-aw-runtime`, wiremock): fake admin serving mcp-servers + fake MCP
  server; an agent whose `tools` allowlist includes an `mcp:<server>` ref → the LLM is offered the
  tool, calling it returns the fake server's output; server-down → tool omitted, turn still completes.
- runner-host: a thin test that `McpToolSource` is constructed when `GREENTIC_AW_MCP=1` + registry env
  set, `None` otherwise.
- Gates: `bash ci/local_check.sh` in greentic-runner (workspace fmt/clippy/test).

## Out of scope
- Admin/designer **authoring UI** for picking an agent's MCP tools — MCP-4 supports the
  `mcp:<server_id>` wire convention; a UI to populate it is a fast-follow (the agent-config editor in
  designer-admin). Until then, opt-in is authored via the existing `PUT /api/v1/designer/agents/{id}`
  JSON or the `{agent_id}.json` manifest.
- MCP `resources`/`prompts`/`sampling` (tools only); per-tool-call session reuse (v1 pays the
  initialize round trips per call, same as MCP-3); flow-editor role (that's MCP-3); MCP-5 right-click.

## Rollout
Single PR to `greentic-runner` `research`: aw-runtime `mcp_source.rs` + `tools.rs`/`loop.rs` branch +
`AgentRuntime` arg + runner-host wiring + tests + this spec copied into the repo. Version bump happens
on the release commit per repo convention, not in this PR. Runner repin/deploy is a separate op step
(noted alongside the existing `aw-overlay-v1` runner-repin follow-up).
