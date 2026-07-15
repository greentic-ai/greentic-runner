# Test-Chat Real RAG via Runner Sidecar — Design (Epic B)

**Date:** 2026-06-29
**Status:** Design approved (architecture + B0 path). B0 ready for an implementation plan.
**Repos:** `greentic-runner` (B0), `greentic-designer` (B1–B3).

## 1. Background & goal

The DW Composer **test-chat** runs the worker **in-process** in the designer via `greentic-aw-runtime` pinned with the `test-mock` feature (`greentic-designer/Cargo.toml`). It tests LLM + tools with real credentials, but **knowledge/RAG retrieval is inert**: `dw_form_to_agent_config` maps `KnowledgeSettings`, but no `knowledge-chronicle` backend is mounted, so retrieval never happens (the only knowledge path is the static `load_kb_from_pack` prompt-prepend, capped at 30k chars — not vector RAG).

**Goal:** let the operator test a worker with **real RAG retrieval** (the same vector pipeline the deployed runner uses), **without** (a) making the designer build heavy (no Chronicle/SurrealDB/RocksDB compiled into the designer) and (b) standing up manual infra (no NATS, no separately-managed services).

## 2. Architecture — designer-managed runner sidecar

The designer **spawns a `greentic-runner` child process** (a prebuilt binary, built `--features greentic-runner-host/knowledge-chronicle`) that loads the worker's `.gtpack` and serves on loopback. The test-chat turn is **proxied over HTTP** to the sidecar, which runs the real agent loop + real RAG. This mirrors the existing **`SorxDeployManager`** (the designer already spawns + reverse-proxies `greentic-sorx` children for SoRLa deployments).

- **Designer stays light:** only process management + an HTTP client; no knowledge-chronicle deps compiled in.
- **Faithful:** the sidecar is the production runtime; retrieval is the real thing.
- **No NATS / no manual infra:** loopback HTTP; the designer spawns + supervises the child transparently (operator sets nothing up).

**The blocker this design resolves:** the runner today has **no HTTP chat ingress** — its only routes are `/healthz`, `/admin/packs/*`, `/operator/op/invoke` (CBOR, bypasses flow+session), `/sql/*`. The only ways to send a chat turn are NATS (`greentic.agentic.request.v1`) or the in-process Rust API `RunnerHost::handle_activity`. So the keystone slice (**B0**) adds a small HTTP route to the runner that wraps `handle_activity`; everything else is designer-side glue over proven patterns.

## 3. Decomposition

| Slice | Scope | Repo | Notes |
|------|-------|------|-------|
| **B0 — runner chat ingress** | `POST /agent/chat` wrapping `RunnerHost::handle_activity`. **(this spec, detailed below)** | greentic-runner | Keystone; small (~3 files); independently testable via curl against a loaded pack |
| **B1 — RunnerSidecarManager** | designer spawns/supervises a `greentic-runner` child per test session; materializes the worker `.gtpack` + `.gtbind`; lifecycle (port, healthz, exit-watcher) | greentic-designer | Mirrors `SorxDeployManager` |
| **B2 — test-chat dispatch swap** | the test-chat dispatcher proxies the turn to the sidecar's `/agent/chat` (reuse reverse-proxy); FE protocol unchanged | greentic-designer | Swaps the inner `runtime.step(...)` for a sidecar call |
| **B3 — embedding-key + UX** | embedding env plumbing into the sidecar; binary-missing / key-missing graceful UX | greentic-designer | Mirrors sorx `422 sorx_binary_missing` |

B0 ships first (keystone, self-contained). B1–B3 each get their own spec→plan→build.

---

## 4. B0 — runner chat ingress (detailed design)

### 4.1 State plumbing
`ServerState` (`crates/greentic-runner-host/src/runner/mod.rs`) does **not** currently hold the host, so a handler can't reach `handle_activity`. Add it:
- Add field `host: Arc<RunnerHost>` to `ServerState`.
- Thread it through `HostServer::new` / `HostServer::with_sql` signatures.
- At the call site in `lib.rs::run()`, pass `Arc::clone(&host)` (the `RunnerHost` already exists there; today only `host.active_packs()` is passed in).

### 4.2 Route & handler
Register `POST /agent/chat` on the router (alongside the existing routes). Handler pattern mirrors `http/admin.rs`:
```rust
pub async fn agent_chat(
    AdminGuard,                          // loopback-only when ADMIN_TOKEN unset
    State(state): State<ServerState>,
    Json(req): Json<AgentChatRequest>,
) -> impl IntoResponse
```
- **Request** (`AgentChatRequest`, serde): `{ text: String, tenant: Option<String>, conversationId: Option<String>, userId: Option<String>, flowId: Option<String> }`.
- **Tenant resolution:** `req.tenant` else `state.routing.default_tenant` (the single loaded pack's tenant). 404 `tenant_not_loaded` if `handle_activity` reports the tenant isn't active.
- **Build the activity:** `Activity::text(req.text).in_conversation(conv).from_user(user)` where `conv`/`user` default to stable constants (e.g. `"test-chat"`) so omitting them still threads one conversation; `req.flow_id` → `.with_flow(id)` when present (else the pack's entry/`messaging` flow is used by `resolve_flow_id`).
- **Invoke:** `state.host.handle_activity(&tenant, activity).await`.
- **Response** (`AgentChatResponse`): map the returned `Vec<Activity>` → `{ replies: [{ text: String }] }`, reading each reply's `payload()["text"]` (fallback `payload()["messages"][0]["text"]`, else the whole payload as a string). On `Err` → `500` JSON `{ error, message }`.
- **Streaming:** v1 is **blocking** (single JSON response). SSE step-streaming (to mirror the test-chat `frame` events) is a deferred B0-followup; the designer can render the blocking reply first.

### 4.3 Auth
Reuse the existing `AdminGuard` extractor: when `ADMIN_TOKEN` is unset (the sidecar's case), it allows loopback connections and 403s non-loopback — exactly the isolation we want (the designer proxy strips inbound auth; the child binds `127.0.0.1`). No new auth code.

### 4.4 Multi-turn
`handle_activity` threads conversation state by a canonical session hint derived from `tenant:provider:channel:conversation:user` (in-memory `InMemorySessionStore` by default). A second `/agent/chat` with the same `conversationId`+`userId` continues the conversation. No Redis needed for the **flow** session store.

### 4.5 Feature availability
`agentic-worker` is in the runner's default features; `handle_activity` is not feature-gated. `knowledge-chronicle` is the extra build flag for RAG. The route compiles + works in a default build (RAG simply inert until built with the feature + embedding env — same graceful-degrade as the runtime).

### 4.6 Testing (B0)
- An axum handler test: build a `ServerState` with a `RunnerHost` that has a tiny test pack loaded (or a stubbed host), POST `/agent/chat` with `{text}`, assert `200` + a `replies[0].text`. Reuse the runner's existing host/test harness for a loaded pack.
- Loopback auth: a non-loopback request → 403 when `ADMIN_TOKEN` unset (covered by the existing `AdminGuard` tests; add one for the new route if cheap).
- Manual: `greentic-runner --bindings worker.gtbind --port N` then `curl -XPOST 127.0.0.1:N/agent/chat -d '{"text":"hi"}'` → reply.

### 4.7 Files (B0)
- Modify `crates/greentic-runner-host/src/runner/mod.rs` (ServerState field + route + `HostServer::new/with_sql` signature).
- Create `crates/greentic-runner-host/src/http/agent_chat.rs` (handler + DTOs) (+ `mod` wiring).
- Modify `crates/greentic-runner-host/src/lib.rs` (`run()` passes `Arc::clone(&host)`).
- Reuse `http/auth.rs::AdminGuard`, `activity.rs::Activity`, `host.rs::handle_activity`.

---

## 5. B1–B3 sketch (designer)

- **B1 RunnerSidecarManager** (`src/orchestrate/runner_sidecar/`, mirroring `sorx_deploy/`): `GREENTIC_RUNNER_BIN` resolution (+ `runner_binary_available`, `422 runner_binary_missing`); per-session child keyed `tenant:session`; allocate ephemeral port; materialize the worker `.gtpack` from the current `DwFormState` (reuse `dw_application_pack::write_gtpack` + the knowledge corpus) under the storage dir; write a `worker.gtbind` (`pack_locator: fs:///…/pack.gtpack`, the worker's flow); spawn `greentic-runner --bindings worker.gtbind --port N` with the embedding env injected; healthz-wait; exit-watcher; per-id lock; idle teardown (e.g. on panel close / TTL).
- **B2 dispatch swap** (`dw_test_chat/dispatcher.rs`): when sidecar mode is active, ensure a sidecar for the session (build/load pack), then POST each turn to `127.0.0.1:{port}/agent/chat` (reuse the `sorla_deploy_proxy` forwarding pattern) and emit the reply as `TestChatEvent::TextChunk` + `Done`. Keep the FE protocol (POST→session_id, GET SSE `frame`) unchanged. A toggle ("Test with real RAG") or auto-activate when a knowledge binding is set **and** `GREENTIC_RUNNER_BIN` is present; otherwise fall back to today's in-process path.
- **B3 embedding-key + UX**: pass `GREENTIC_KNOWLEDGE_EMBED_{BASE_URL,API_KEY,MODEL}` to the sidecar env (from designer config / tenant settings); binary-missing and key-missing surface as graceful, actionable messages (mirror sorx 422), never a hard test-chat failure.

## 6. Risks / open questions
- **R1 (agent state store — confirm in B1):** the runner's `dw.agent` node may require `GREENTIC_AW_REDIS_URL` for the agent's Plan-Act-Observe state (the in-process designer test-chat uses a mock store and needs none). Before B1, confirm whether the runner has an in-memory fallback for the agent state store; if Redis is genuinely mandatory it conflicts with the "no infra" goal and we choose a mitigation (enable a memory backend, or bundle a throwaway store) — **explicitly a B1 gate, not a B0 concern.**
- **R2 (runner binary build):** the RAG sidecar binary must be built `--features greentic-runner-host/knowledge-chronicle` (needs `clang` for RocksDB; cross-org git access to chronicle-ext). One-time; resolved via `GREENTIC_RUNNER_BIN`.
- **R3 (embedding key for live verify):** real retrieval needs an embedding endpoint; without it the tier degrades gracefully (no crash), but the live RAG test can't be fully verified — same external dependency as S4.
- **R4 (pack materialization cost):** building a `.gtpack` per test session adds latency; mitigate by caching per (session, form-hash) and only rematerializing when the form/corpus changes.

## 7. Success criteria
- **B0:** a `greentic-runner` serving a loaded worker pack answers `POST /agent/chat {text}` with the worker's reply over loopback HTTP; multi-turn threads by `conversationId`; non-loopback is rejected. Covered by a handler test + manual curl.
- **Epic B (end state):** in the DW Composer, an operator with a knowledge-bound worker runs the test-chat and gets answers grounded in real vector retrieval from the attached corpus (verifiable: an answer only derivable from an uploaded doc), with the designer build unchanged and no manual infra — given a `GREENTIC_RUNNER_BIN` (RAG build) + embedding key.
