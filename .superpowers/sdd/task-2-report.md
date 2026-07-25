# Task 2 Report: POST /agent/chat ingress

## Status: COMPLETE ✓

## Commit
`7bd916b0` — `feat(runner): POST /agent/chat ingress wrapping handle_activity (loopback-only)`

## TDD cycle
**RED** → Wrote `agent_chat_unknown_tenant_maps_to_not_found` test in `agent_chat.rs` calling `execute_chat` against a host with no loaded packs. The test referenced `execute_chat` and `RunnerHost::for_test()` which didn't exist yet, so it would not have compiled/passed before the implementation.

**GREEN** → After wiring all changes:
```
cargo test -p greentic-runner-host agent_chat
running 5 tests
test http::agent_chat::tests::maps_text_payload_to_reply ... ok
test http::agent_chat::tests::maps_nested_messages_text ... ok
test http::agent_chat::tests::skips_empty_and_keeps_order ... ok
test http::agent_chat::tests::request_deserializes_camel_case ... ok
test http::agent_chat::tests::agent_chat_unknown_tenant_maps_to_not_found ... ok
test result: ok. 5 passed; 0 failed
```

## Test approach chosen
Unit test of `execute_chat` (extracted core) rather than full oneshot route test. Reason: building a full `ServerState` in tests requires a real `RunnerHost` which has no cheap default constructor; `AdminGuard` also requires `ConnectInfo<SocketAddr>` extension injected by axum's serve layer, making oneshot tests heavyweight. The `execute_chat` extraction directly covers the critical mapping: `handle_activity` "not loaded" error → `StatusCode::NOT_FOUND` + `error:"tenant_not_loaded"`.

## Files changed (8)
- `src/routing.rs` — added `pub fn default_tenant(&self) -> &str` getter on `TenantRouting` (field is private)
- `src/runner/mod.rs` — added `use crate::host::RunnerHost`; `pub host: Arc<RunnerHost>` to `ServerState`; `host: Arc<RunnerHost>` param to `HostServer::new` and `HostServer::with_sql`; `.route("/agent/chat", post(crate::http::agent_chat::agent_chat))` registered
- `src/lib.rs` — threaded `Arc::clone(&host)` into `HostServer::new` at the `run()` call site
- `src/host.rs` — added `#[cfg(test)] pub(crate) fn for_test() -> Arc<Self>` on `RunnerHost` (minimal host with one dummy tenant config, no packs loaded, uses real session/state stores and `default_manager()`)
- `src/http/agent_chat.rs` — added `execute_chat` core fn, `agent_chat` handler, and the new route test
- `src/http/admin.rs` / `auth.rs` / `health.rs` — updated test `state()` helpers to include `host: RunnerHost::for_test()`

## Key signatures threaded
- `HostServer::new(port, active, routing, health, reload, admin, host: Arc<RunnerHost>) -> Result<Self>`
- `HostServer::with_sql(port, active, routing, health, reload, admin, host: Arc<RunnerHost>, sql_gateway: Option<SqlGateway>) -> Result<Self>`
- `TenantRouting::default_tenant(&self) -> &str` (new method)
- Not-loaded error: `handle_activity` returns `"tenant {name} not loaded"` (string via `anyhow::Context`); handler checks `msg.contains("not loaded") || msg.contains("not registered")` → 404

## Build verification
```
cargo build -p greentic-runner   → Finished (0 errors)
cargo clippy -p greentic-runner-host --all-targets → 2 warnings only:
  - unused_variables in agent_node.rs (pre-existing, not my code)
  - too_many_arguments on with_sql (pre-existing: already had 7 args; now 8)
cargo test -p greentic-runner-host agent_chat → 5/5 PASS
```

## Self-review
- `host` threaded through both `HostServer::new` AND `HostServer::with_sql`, and stored in `ServerState` ✓
- Route `.route("/agent/chat", post(...))` registered in router chain ✓
- `AdminGuard` applied as first handler arg (`_guard: AdminGuard`) ✓
- Tenant resolution: explicit `req.tenant` takes precedence, else `state.routing.default_tenant()` ✓
- Error mapping: `not loaded` → 404 + `tenant_not_loaded`; anything else → 500 + `agent_chat_failed` ✓
- No `unwrap()`/`panic!()` on the request path ✓
- All 3 existing test `state()` helpers updated to include `host` field ✓

## Concerns
- `with_sql` now has 8 parameters triggering `clippy::too_many_arguments`. Pre-existing pattern (was 7 before); suppressing with `#[allow(...)]` or grouping into a builder struct would be a separate cleanup.
- The not-loaded error is matched by string (`contains("not loaded")`). A typed error variant would be cleaner but `handle_activity` uses `anyhow::Context` strings rather than a typed enum — this is consistent with how `TenantRuntimeHandle` in `routing.rs` handles the same case.

---

## Review-findings fix (Task 2b)

### Fix 1 — clippy too_many_arguments
**File:** `crates/greentic-runner-host/src/runner/mod.rs` (immediately above `pub fn with_sql`, line ~69)
Added a one-line justification comment plus `#[allow(clippy::too_many_arguments)]`:
```rust
// Builder param set mirrors the existing fields plus `host`; a config-struct
// refactor is deferred. See B0 plan.
#[allow(clippy::too_many_arguments)]
pub fn with_sql(
```

### Fix 2 — over-broad not-loaded match
**File:** `crates/greentic-runner-host/src/http/agent_chat.rs` (line ~113)
Dropped `|| msg.contains("not registered")` — now only `msg.contains("not loaded")` maps to 404/tenant_not_loaded. The "not registered" substring was too generic and could mis-map unrelated flow/component errors.
Before: `if msg.contains("not loaded") || msg.contains("not registered")`
After:  `if msg.contains("not loaded")`

### Clippy output (post-fix)
```
warning: unused variable: `store_resolved`
   --> crates/greentic-runner-host/src/runner/agent_node.rs:664:13
warning: `greentic-runner-host` (lib) generated 1 warning
Finished `dev` profile
```
`too_many_arguments` gone. Only the pre-existing `store_resolved` warning in `agent_node.rs` remains (not touched).

### Test output (5/5 pass)
```
running 5 tests
test http::agent_chat::tests::skips_empty_and_keeps_order ... ok
test http::agent_chat::tests::maps_text_payload_to_reply ... ok
test http::agent_chat::tests::maps_nested_messages_text ... ok
test http::agent_chat::tests::request_deserializes_camel_case ... ok
test http::agent_chat::tests::agent_chat_unknown_tenant_maps_to_not_found ... ok
test result: ok. 5 passed; 0 failed; 0 ignored
```
