# Runner Agent-Chat Ingress (B0) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Add a small `POST /agent/chat` HTTP route to the greentic-runner host that wraps `RunnerHost::handle_activity`, so an external loopback caller (the designer's runner sidecar, later slices) can send a chat turn to a loaded agentic-worker pack and get the reply.

**Architecture:** Add `Arc<RunnerHost>` to the axum `ServerState`, register a `POST /agent/chat` route guarded by the existing loopback-only `AdminGuard`, and a handler that maps a JSON request to an `Activity`, calls `handle_activity`, and maps the returned `Vec<Activity>` to a JSON reply via a pure helper.

**Tech Stack:** Rust, axum, serde, tokio. greentic-runner-host crate.

## Global Constraints

- **English only** in source/tests/comments/commits. No `unwrap()`/`panic!()` on the request path (handlers return errors as responses; tests may unwrap).
- Follow the existing runner-host patterns: handler signature style of `crates/greentic-runner-host/src/http/admin.rs`; auth via the existing `AdminGuard` extractor (`crates/greentic-runner-host/src/http/auth.rs`) — when `ADMIN_TOKEN` is unset it allows loopback and 403s non-loopback (exactly what the sidecar needs). Do NOT invent new auth.
- Request/response JSON is **camelCase** (`#[serde(rename_all = "camelCase")]`).
- The route must compile and work in a **default** build (`agentic-worker` is default; `handle_activity` is not feature-gated). Do NOT require the `knowledge-chronicle` feature to build/test B0.
- Conventional Commits. NO Claude co-author trailer.
- **Build env:** `export CARGO_TARGET_DIR=/home/bima-pangestu/.cache/greentic-runner-target` before cargo (shared incremental cache; avoids a cold build). Test with `cargo test -p greentic-runner-host <filter>` (default features — no clang/RocksDB needed since knowledge-chronicle stays off).

## File Structure
- **Create** `crates/greentic-runner-host/src/http/agent_chat.rs` — `AgentChatRequest`/`AgentChatResponse`/`ReplyView` DTOs + a pure `replies_to_response(Vec<Activity>) -> AgentChatResponse` mapper + the `agent_chat` axum handler.
- **Modify** `crates/greentic-runner-host/src/http/mod.rs` (or wherever `http` submodules are declared) — add `pub mod agent_chat;`.
- **Modify** `crates/greentic-runner-host/src/runner/mod.rs` — add `host: Arc<RunnerHost>` to `ServerState`, thread it through `HostServer::new`/`with_sql`, register the `POST /agent/chat` route.
- **Modify** `crates/greentic-runner-host/src/lib.rs` — pass `Arc::clone(&host)` into the `HostServer` constructor at the `run()` call site.

---

## Task 1: Reply mapping + DTOs (pure)

**Files:**
- Create: `crates/greentic-runner-host/src/http/agent_chat.rs`
- Modify: `crates/greentic-runner-host/src/http/mod.rs` (add `pub mod agent_chat;`)

**Interfaces produced:**
- `pub struct AgentChatRequest { text: String, tenant: Option<String>, conversation_id: Option<String>, user_id: Option<String>, flow_id: Option<String> }` (serde camelCase, Deserialize)
- `pub struct ReplyView { text: String }` (serde, Serialize)
- `pub struct AgentChatResponse { replies: Vec<ReplyView> }` (serde, Serialize)
- `pub fn replies_to_response(activities: Vec<Activity>) -> AgentChatResponse` — extract reply text from each activity: prefer `payload()["text"]` as a string; else `payload()["messages"][0]["text"]`; else the whole payload rendered compactly (`serde_json::to_string`); skip activities that yield an empty string.

**Before coding:** read `crates/greentic-runner-host/src/activity.rs` for the exact `Activity` accessor for its payload (the exploration found `activity.payload() -> &serde_json::Value`); use the real method name. Read the top of an existing `http/*.rs` file for the import/style conventions.

- [ ] **Step 1: Write the failing test** — in `agent_chat.rs`, a `#[cfg(test)]` module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Activity;
    use serde_json::json;

    fn reply_with(payload: serde_json::Value) -> Activity {
        // Build an outbound-style activity carrying `payload`. Use the same
        // constructor the runner uses for replies (Activity::from_output) —
        // read activity.rs and match it; here we assert on the mapping only.
        Activity::from_output(payload, "demo")
    }

    #[test]
    fn maps_text_payload_to_reply() {
        let out = replies_to_response(vec![reply_with(json!({"text": "hello there"}))]);
        assert_eq!(out.replies.len(), 1);
        assert_eq!(out.replies[0].text, "hello there");
    }

    #[test]
    fn maps_nested_messages_text() {
        let out = replies_to_response(vec![reply_with(json!({"messages": [{"text": "nested hi"}]}))]);
        assert_eq!(out.replies[0].text, "nested hi");
    }

    #[test]
    fn skips_empty_and_keeps_order() {
        let out = replies_to_response(vec![
            reply_with(json!({"text": ""})),
            reply_with(json!({"text": "second"})),
        ]);
        assert_eq!(out.replies.len(), 1);
        assert_eq!(out.replies[0].text, "second");
    }

    #[test]
    fn request_deserializes_camel_case() {
        let r: AgentChatRequest = serde_json::from_value(json!({
            "text": "hi", "conversationId": "c1", "userId": "u1"
        })).unwrap();
        assert_eq!(r.text, "hi");
        assert_eq!(r.conversation_id.as_deref(), Some("c1"));
        assert_eq!(r.user_id.as_deref(), Some("u1"));
    }
}
```

> If `Activity::from_output` is private, use the public constructor that sets the payload (read `activity.rs`; the exploration noted `Activity::custom("response", payload)` is what `from_output` wraps). Match the real API — the assertions on `replies_to_response` are the contract.

- [ ] **Step 2: Run (fail)** — `export CARGO_TARGET_DIR=/home/bima-pangestu/.cache/greentic-runner-target; cargo test -p greentic-runner-host agent_chat 2>&1 | tail -15` → FAIL (items not found).

- [ ] **Step 3: Implement** — prepend to `agent_chat.rs`:

```rust
//! `POST /agent/chat` — a loopback HTTP ingress that wraps `RunnerHost::handle_activity`
//! so an external caller (the designer's runner sidecar) can send a chat turn to a
//! loaded agentic-worker pack and receive the reply. Blocking JSON response (v1).

use serde::{Deserialize, Serialize};

use crate::activity::Activity;

/// One chat turn for a loaded worker pack.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatRequest {
    pub text: String,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub flow_id: Option<String>,
}

/// One outbound reply line.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplyView {
    pub text: String,
}

/// The worker's reply turn.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChatResponse {
    pub replies: Vec<ReplyView>,
}

/// Extract a human-readable reply line from each outbound activity.
fn reply_text(activity: &Activity) -> String {
    let payload = activity.payload();
    if let Some(t) = payload.get("text").and_then(|v| v.as_str()) {
        return t.to_string();
    }
    if let Some(t) = payload
        .get("messages")
        .and_then(|m| m.get(0))
        .and_then(|m0| m0.get("text"))
        .and_then(|v| v.as_str())
    {
        return t.to_string();
    }
    serde_json::to_string(payload).unwrap_or_default()
}

/// Map the runtime's outbound activities into the chat response, dropping empties.
pub fn replies_to_response(activities: Vec<Activity>) -> AgentChatResponse {
    let replies = activities
        .iter()
        .map(reply_text)
        .filter(|t| !t.trim().is_empty())
        .map(|text| ReplyView { text })
        .collect();
    AgentChatResponse { replies }
}
```

Add `pub mod agent_chat;` to `crates/greentic-runner-host/src/http/mod.rs` (match the existing `pub mod health;` / `pub mod admin;` lines).

- [ ] **Step 4: Run (pass)** — `cargo test -p greentic-runner-host agent_chat 2>&1 | tail -15` → PASS (4 tests). `cargo fmt`.

- [ ] **Step 5: Commit**
```bash
git add crates/greentic-runner-host/src/http/agent_chat.rs crates/greentic-runner-host/src/http/mod.rs
git commit -m "feat(runner): agent-chat reply mapping + request/response DTOs"
```

---

## Task 2: Wire ServerState.host + route + handler + integration test

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/mod.rs` (ServerState field, route, HostServer ctor signature)
- Modify: `crates/greentic-runner-host/src/lib.rs` (pass `Arc::clone(&host)` at the `run()` call site)
- Modify: `crates/greentic-runner-host/src/http/agent_chat.rs` (add the `agent_chat` handler)

**Interfaces:**
- Consumes: `replies_to_response` (Task 1); `RunnerHost::handle_activity(&self, tenant: &str, activity: Activity) -> anyhow::Result<Vec<Activity>>`; `Activity::text(..).in_conversation(..).from_user(..).with_flow(..)`; the `AdminGuard` extractor; `ServerState`.
- Produces: route `POST /agent/chat` → `200 AgentChatResponse` | `404 {error:"tenant_not_loaded"}` | `500 {error,message}` | `403` (non-loopback).

**Before coding — READ for exact signatures (the exploration mapped these, but confirm verbatim):**
- `crates/greentic-runner-host/src/runner/mod.rs`: the `ServerState` struct (fields `active`, `routing`, `health`, `reload`, `admin`, `sql`) and `HostServer::new` / `HostServer::with_sql` signatures + where `.route(...)` chains are built + `.with_state(state)`.
- `crates/greentic-runner-host/src/lib.rs`: the `run()` site that constructs `HostServer` (today passes `host.active_packs()`); the `host` binding type is `Arc<RunnerHost>` (or wrap it: read it).
- `crates/greentic-runner-host/src/http/admin.rs`: a handler signature using `AdminGuard` + `State<ServerState>` to copy the exact form.
- `crates/greentic-runner-host/src/routing.rs`: `TenantRouting` — how to get a default tenant (the exploration referenced `state.routing.default_tenant`; confirm the real accessor/field name).

- [ ] **Step 1: Write the failing integration test** — add to `agent_chat.rs`'s test module a route-level test. Use the runner-host's existing test harness for constructing a `ServerState`/`HostServer` (search the crate for existing axum route tests, e.g. a health-route test, and mirror its setup). The test asserts the **wiring + auth + error path** without needing a fully RAG-loaded pack:

```rust
    // Route-level: a loopback POST to /agent/chat with no loaded tenant returns 404
    // tenant_not_loaded (proves the route is wired and reaches handle_activity), and
    // the body is our JSON error shape. Mirror the existing health/admin route test setup.
    #[tokio::test]
    async fn agent_chat_route_wired_and_handles_unknown_tenant() {
        // Build a ServerState with no packs loaded using the crate's test helper
        // (see the existing route tests in runner/mod.rs or http/*). Then:
        //   let app = router_with_state(state);
        //   let res = app.oneshot(POST /agent/chat, json!({"text":"hi","tenant":"nope"})).await;
        //   assert_eq!(res.status(), 404);
        //   assert_eq!(body["error"], "tenant_not_loaded");
        // If no in-crate test harness exists to build ServerState cheaply, instead unit-test
        // the handler's tenant-resolution + error-mapping by calling a small extracted
        // `async fn run_chat(host: &RunnerHost, req: AgentChatRequest) -> Result<AgentChatResponse, ChatError>`
        // against a RunnerHost with no packs, asserting ChatError::TenantNotLoaded.
    }
```

> Pick whichever is cheaper given the crate's existing test scaffolding: a full `oneshot` route test (preferred — proves the route + AdminGuard) or a handler-core unit test (`run_chat`) if building a `ServerState` in tests is heavyweight. Either MUST exercise the tenant-not-loaded → 404 mapping. Read the crate's existing tests first to decide.

- [ ] **Step 2: Run (fail)** — `cargo test -p greentic-runner-host agent_chat 2>&1 | tail -20` → FAIL.

- [ ] **Step 3a: Thread the host into ServerState** — in `runner/mod.rs`:
  - Add `pub host: std::sync::Arc<crate::host::RunnerHost>,` to `ServerState`.
  - Add a `host: Arc<RunnerHost>` parameter to `HostServer::new` and `HostServer::with_sql`, and store it in the `ServerState { … host, … }` construction.
  - In `lib.rs::run()`, pass `std::sync::Arc::clone(&host)` (read the exact `host` binding; it is the `RunnerHost` the server is built from).
  - Register the route in the router chain: `.route("/agent/chat", post(crate::http::agent_chat::agent_chat))` (import `axum::routing::post` if not already).

- [ ] **Step 3b: Implement the handler** — append to `agent_chat.rs`:

```rust
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};

use crate::http::auth::AdminGuard;
use crate::runner::ServerState;

/// Default conversation/user identifiers so a caller that omits them still threads
/// a single in-memory conversation across turns.
const DEFAULT_CONVERSATION: &str = "test-chat";
const DEFAULT_USER: &str = "test-chat-user";

/// `POST /agent/chat` — loopback-only (AdminGuard). Sends one chat turn to the
/// loaded worker pack and returns its reply.
pub async fn agent_chat(
    _guard: AdminGuard,
    State(state): State<ServerState>,
    Json(req): Json<AgentChatRequest>,
) -> impl IntoResponse {
    let tenant = req
        .tenant
        .clone()
        .unwrap_or_else(|| state.routing.default_tenant().to_string());

    let mut activity = Activity::text(req.text)
        .in_conversation(req.conversation_id.unwrap_or_else(|| DEFAULT_CONVERSATION.to_string()))
        .from_user(req.user_id.unwrap_or_else(|| DEFAULT_USER.to_string()));
    if let Some(flow) = req.flow_id {
        activity = activity.with_flow(flow);
    }

    match state.host.handle_activity(&tenant, activity).await {
        Ok(activities) => (StatusCode::OK, Json(replies_to_response(activities))).into_response(),
        Err(e) => {
            let msg = format!("{e:#}");
            // handle_activity errors when the tenant isn't loaded; surface that distinctly.
            let (code, error) = if msg.contains("tenant") || msg.contains("not loaded") || msg.contains("not found") {
                (StatusCode::NOT_FOUND, "tenant_not_loaded")
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "agent_chat_failed")
            };
            (code, Json(serde_json::json!({ "error": error, "message": msg }))).into_response()
        }
    }
}
```

> Confirm the real `default_tenant` accessor on `TenantRouting` and the `RunnerHost` field name on `ServerState` while reading the files. If the tenant-not-loaded error from `handle_activity` carries a typed variant rather than a message, match on that instead of the string `contains` (cleaner — prefer it if available).

- [ ] **Step 4: Run (pass)** — `cargo test -p greentic-runner-host agent_chat 2>&1 | tail -20` → PASS. Then `cargo build -p greentic-runner 2>&1 | tail -5` (the binary still builds with the new state field) and `cargo clippy -p greentic-runner-host --all-targets 2>&1 | tail -5` clean. `cargo fmt`.

- [ ] **Step 5: Commit**
```bash
git add crates/greentic-runner-host/src/runner/mod.rs crates/greentic-runner-host/src/lib.rs crates/greentic-runner-host/src/http/agent_chat.rs
git commit -m "feat(runner): POST /agent/chat ingress wrapping handle_activity (loopback-only)"
```

---

## Self-Review

- **Spec coverage:** §4.1 state plumbing (Task 2 Step 3a); §4.2 route+handler+DTOs (Task 1 DTOs+mapper, Task 2 handler); §4.3 AdminGuard auth (Task 2 handler `_guard: AdminGuard`); §4.4 multi-turn via conversationId (handler defaults + `in_conversation`); §4.6 testing (Task 1 mapper unit tests + Task 2 route/error test). ✅
- **Placeholder scan:** code is complete for the DTOs/mapper/handler; the two "read the file to confirm the exact signature" callouts are concrete (named file + named symbol) because the precise current signatures of `HostServer::new`/`ServerState`/`default_tenant` are cross-repo and must be matched verbatim — not invented.
- **Type consistency:** `AgentChatRequest`/`AgentChatResponse`/`ReplyView`/`replies_to_response` identical across Task 1 (def) and Task 2 (use). Handler returns the spec's `200 AgentChatResponse` / `404 tenant_not_loaded` / `500`.

## Notes
- B0 deliberately does NOT touch knowledge-chronicle, the sidecar, or the designer — it only adds the ingress so later slices (B1 sidecar, B2 dispatch) can use it. RAG correctness is verified end-to-end in B1/B2 with a real built runner + embedding key.
- Streaming (SSE `frame` events to match the test-chat protocol) is a deferred B0 follow-up; the blocking JSON reply is sufficient for the designer to render a turn.
