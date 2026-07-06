# Mandatory Guardrail Policy — Runner Provider (Slice 2b)

- **Date:** 2026-06-20
- **Status:** Design / approved for planning
- **Repo:** `greentic-runner` (crates `greentic-aw-runtime` + `greentic-runner-host`)
- **Branch:** `feat/guardrail-policy-provider` (off `research`) → PR to `research`
- **Related:** [[project_guardrail_capability]]. Consumes the Slice 2a admin endpoint (greentic-admin PR #225, merged `research`). Runtime base = PR #465 (`130b458`).

## 1. Background

Guardrails are WASM-capability extensions the agentic worker runs around each step. Slice 1+3 (designer) let an author attach guardrails to one agent. Slice 2a (admin) lets an operator define **mandatory** guardrails per tenant/env/global and serves the resolved union at `GET /api/v1/designer/guardrail-policy?env=` (gtc_live_ bearer, tenant implied by token), returning `{ "guardrails": [ { "cap_id", "config" } ] }` (snake_case).

The runner already has the enforcement machinery (PR #465):

- `GuardrailPolicy` trait (`greentic-aw-runtime/src/guardrail.rs:72`) — `mandatory_guardrails(&self, tenant: &TenantContext) -> Vec<GuardrailRef>` (currently **sync, infallible**).
- The agent loop (`greentic-aw-runtime/src/loop.rs:60-75`) calls it, then `assemble_chain(registry, &mandatory, &config.guardrails)`; an unresolvable mandatory cap → `Err(unresolved)` → `AgentError::GuardrailDenied { code: "internal", message: "A required guardrail is unavailable." }` (fail-closed, already implemented).
- The build site (`greentic-runner-host/src/runner/agent_node.rs:727`) wires `StaticGuardrailPolicy(mandatory_guardrails_from_config())` into the `AgentRuntime`. The seam `mandatory_guardrails_from_config()` (`agent_node.rs:144`) returns `Vec::new()` (TODO stub).

**This slice (2b)** replaces the stub with a real provider that fetches the mandatory list from the admin per tenant+env, caches it, serves stale on transient admin failure, and fails closed on a cold outage.

## 2. Scope

**In scope:** the `GuardrailPolicy` trait change (async + fallible), the existing impls + the one consumer updated, a new `HttpGuardrailPolicy`, and the build-site wiring. One PR to `greentic-runner` `research`.

**Out of scope:**
- Admin storage / endpoint / UI — shipped in Slice 2a.
- **Team** scope — `TenantContext` carries only `tenant_id` + `env_id` (no team); deferred with Slice 2a.
- Deploying the runner with the env vars (ops task, noted in §8).
- Designer re-pin to the post-2b rev — downstream coordination (§8), not part of this PR.

## 3. Decisions (locked with Bima)

| Decision | Choice |
| --- | --- |
| Fail-closed shape | Trait becomes `async fn … -> Result<Vec<GuardrailRef>, GuardrailPolicyError>`. Cold-fetch-failure → `Err(Unavailable)` → the consumer maps it to `AgentError::GuardrailDenied` (the same denied UX as an unresolvable mandatory cap). |
| Cache / stale | 60s TTL (matches the config/graph cache, "Decision 13"). On fetch error **with** a cached entry (even past TTL) → serve last-known + `warn!` (fail-safe). Block (fail-closed) **only** when there is no cached entry (cold). Stale window is unbounded. |
| Slicing | One spec / one PR to `greentic-runner` `research`. |
| Env-unset fallback | If `GREENTIC_AW_ADMIN_ENDPOINT`/`_TOKEN` are unset, wire `StaticGuardrailPolicy(Vec::new())` (today's behavior — non-admin deploys are never blocked). Only an admin-configured runner fetches + fails-closed. |

## 4. Architecture

### 4.1 Trait change (`greentic-aw-runtime/src/guardrail.rs`)

```rust
#[derive(Debug, thiserror::Error)]
pub enum GuardrailPolicyError {
    /// The mandatory policy could not be obtained and no cached copy exists.
    #[error("mandatory guardrail policy unavailable: {0}")]
    Unavailable(String),
}

#[async_trait::async_trait]
pub trait GuardrailPolicy: Send + Sync {
    async fn mandatory_guardrails(
        &self,
        tenant: &TenantContext,
    ) -> Result<Vec<GuardrailRef>, GuardrailPolicyError>;
}
```

`StaticGuardrailPolicy(pub Vec<GuardrailRef>)` and the no-op impl become `async`, returning `Ok(self.0.clone())` / `Ok(Vec::new())`. (The codebase already uses `#[async_trait::async_trait]` — e.g. `knowledge.rs`, `llm_credential.rs` — so this is idiomatic.)

### 4.2 Consumer (`greentic-aw-runtime/src/loop.rs:61`)

```rust
let mandatory = match runtime.guardrail_policy.mandatory_guardrails(&tenant).await {
    Ok(m) => m,
    Err(e) => {
        warn!(error = %e, "mandatory guardrail policy unavailable; failing closed");
        return Err(AgentError::GuardrailDenied {
            direction: crate::guardrail::GuardrailDirection::Inbound,
            code: "internal".to_string(),
            message: "A required guardrail is unavailable.".to_string(),
            details: serde_json::to_string(&serde_json::json!({ "policy_unavailable": true })).ok(),
        });
    }
};
// unchanged: assemble_chain(&registry, &mandatory, &config.guardrails) → Ok | Err(unresolved)
```

Cold-policy-failure and unresolvable-mandatory-cap now land on the same `GuardrailDenied` outbound shape — one fail-closed UX.

### 4.3 `HttpGuardrailPolicy` (new, `greentic-aw-runtime/src/guardrail_provider.rs`)

Mirrors `HttpConfigProvider` / `CachingGraphProvider` (`graph/http_provider.rs`):

- Fields: `client: reqwest::Client` (10s timeout), `base_url: String`, `token: String`, `ttl: Duration` (default 60s), `cache: Mutex<HashMap<(String,String), (Instant, Vec<GuardrailRef>)>>` keyed by `(tenant_id, env_id)`.

  *Time note:* the existing cache uses `Instant`/`std::time` for TTL; reuse the exact same mechanism as `CachingGraphProvider` (do not introduce a different clock).
- `mandatory_guardrails(tenant)`:
  1. Key = `(tenant.tenant_id, tenant.env_id)`. If cached and `age < ttl` → return clone (no HTTP).
  2. Else `GET {base_url}/api/v1/designer/guardrail-policy?env={env}` with `.bearer_auth(token)`, 10s timeout.
  3. On `200` + valid body → parse, **update cache** (stamp now), return.
  4. On any failure (network, non-200, decode error): if a cached entry exists (any age) → `warn!` + return the stale clone; else → `Err(GuardrailPolicyError::Unavailable(reason))`.

Response model:

```rust
#[derive(serde::Deserialize)]
struct PolicyResp { guardrails: Vec<greentic_aw_runtime::config::GuardrailRef> }
```

`GuardrailRef` already deserializes `{cap_id, config}` (with `offer_id` `#[serde(default)]` → `None`, `config` `#[serde(default)]`). A malformed 200 body → treat as a fetch failure (step 4), mirroring `HttpConfigProvider`'s decode-error→Misconfigured convention.

`new(base_url, token)` → 60s TTL; `with_ttl(...)` for tests.

### 4.4 Wiring (`greentic-runner-host/src/runner/agent_node.rs`)

Remove the `mandatory_guardrails_from_config()` stub. At the build site (`:727`), choose the policy from env (reuse the `registry_from_env()` pattern that reads `GREENTIC_AW_ADMIN_ENDPOINT` + `GREENTIC_AW_ADMIN_TOKEN`):

```rust
let guardrail_policy: Arc<dyn GuardrailPolicy> = match guardrail_policy_from_env() {
    Some(http) => Arc::new(http),                                   // HttpGuardrailPolicy
    None => Arc::new(StaticGuardrailPolicy(Vec::new())),           // today's behavior
};
// .with_guardrails(guardrail_policy, Arc::new(ExtRuntimeGuardrailEvaluator { ... }))
```

`guardrail_policy_from_env()` returns `Some(HttpGuardrailPolicy::new(endpoint, token))` when both env vars are set/non-empty, else `None` — identical gating to `registry_from_env()`. Apply at every build site that currently passes `StaticGuardrailPolicy(mandatory_guardrails_from_config())`.

## 5. Error handling / edge cases

- **Empty policy** (`200 {"guardrails":[]}`) → `Ok(vec![])` → no mandatory guardrails → agent runs normally (NOT blocked). This is the common case for tenants with no mandatory policy.
- **Cold + admin unreachable** → `Err(Unavailable)` → `GuardrailDenied` (fail-closed).
- **Transient admin failure with a warm cache** → serve last-known + `warn!` (fail-safe: stale mandatory guardrails keep protecting; a newly-*added* mandatory guardrail simply isn't picked up until admin recovers — over-protective, never under).
- **Malformed 200 body** → fetch failure (stale-or-cold), never a silent empty list.
- **Env vars unset** → `StaticGuardrailPolicy(empty)` (no enforcement, no blocking).

## 6. Testing

Mirror the `http_provider.rs` wiremock tests:

- `fetch_returns_refs` — 200 with two guardrails → `Ok(vec)` with correct cap_ids/config.
- `cache_hit_skips_second_http` — two calls within TTL → server hit once.
- `ttl_expiry_refetches` — `with_ttl(0)` (or a tiny TTL) → second call re-hits.
- `empty_policy_is_ok_empty` — `200 {"guardrails":[]}` → `Ok(vec![])` (not an error).
- `cold_failure_is_unavailable` — server 500/unreachable, no prior cache → `Err(Unavailable)`.
- `transient_failure_serves_stale` — warm the cache (200), then server 500 → returns the stale refs (Ok), server still recorded the second hit.
- `malformed_body_cold_is_unavailable` — 200 with a non-JSON / wrong-shape body, no cache → `Err`.
- `StaticGuardrailPolicy`/no-op async impls return `Ok(...)` (update the existing guardrail.rs unit tests for the async signature).
- Consumer: a loop-level test (or a focused unit) asserting `Err(Unavailable)` from the policy maps to `AgentError::GuardrailDenied` before `assemble_chain` runs.

## 7. Constraints & gotchas

- `greentic-aw-runtime` pins to Rust 1.95 (`130b458`). `bash ci/local_check.sh` is the gate (fmt + clippy `-D warnings` + test). `#![forbid(unsafe_code)]`; no `unwrap()`/`panic!()` in production paths.
- The trait change is **breaking** for `greentic-aw-runtime`'s public API — every `GuardrailPolicy` impl and the single consumer must move to async in the same PR (in-repo: `StaticGuardrailPolicy`, the no-op, `loop.rs`). Grep the workspace for `mandatory_guardrails` and `GuardrailPolicy` to catch all sites.
- Reuse the existing cache/clock mechanism from `CachingGraphProvider`; do not introduce a second TTL pattern.
- Response shape MUST match Slice 2a exactly (`{guardrails:[{cap_id,config}]}`, snake_case) — it is a merged, fixed contract.
- English only; Conventional Commits; no AI attribution.

## 8. Downstream (not in this PR)

- **Designer re-pin:** designer pins `greentic-aw-runtime` at the guardrail rev. After 2b merges, designer re-pins to the new rev; any designer site constructing a `GuardrailPolicy` (e.g. the test-chat runtime, which uses an empty `StaticGuardrailPolicy`) adapts to the async signature (trivial: add `.await`, impls already return `Ok`). Coordinate as a follow-up designer PR.
- **Deploy:** the runner must run with `GREENTIC_AW_ADMIN_ENDPOINT` + `GREENTIC_AW_ADMIN_TOKEN` pointed at the admin for enforcement to activate (ops). Without them, no mandatory enforcement (safe default).
- **Team scope:** when `TenantContext` gains a team, extend the cache key + the `?env=` call to carry team, and Slice 2a's resolution to include team.
