# Mandatory Guardrail Policy — Runner Provider (Slice 2b) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the runner's `mandatory_guardrails_from_config()` stub with a real provider that fetches the mandatory guardrail list from greentic-admin per tenant+env, caches it (60s TTL, serve-stale, block-only-cold), and fails closed on a cold admin outage.

**Architecture:** Make `GuardrailPolicy::mandatory_guardrails` async + fallible (`Result<Vec<GuardrailRef>, GuardrailPolicyError>`); the loop maps `Err` to the existing `GuardrailDenied` fail-closed UX. Add `HttpGuardrailPolicy` (mirrors `HttpConfigProvider`, with an internal TTL cache like `CachingGraphProvider`). Wire it at the runner-host build site from `GREENTIC_AW_ADMIN_ENDPOINT`/`_TOKEN`, falling back to an empty `StaticGuardrailPolicy` when unset.

**Tech Stack:** Rust 1.95, `reqwest`, `tokio`, `wiremock` (tests), `thiserror`. **The `GuardrailPolicy` trait is object-safe (used as `Arc<dyn GuardrailPolicy>`), so its async method returns a manual `Pin<Box<dyn Future + Send>>` — mirroring the `ConfigProvider` trait in `http_provider.rs`. Do NOT use `#[async_trait]`: `crates/greentic-aw-runtime/Cargo.toml` explicitly forbids it on AW traits.**

## Global Constraints

- `greentic-aw-runtime` and `greentic-runner-host` pin to Rust 1.95.0. `#![forbid(unsafe_code)]`; no `unwrap()`/`panic!()` in production paths (tests may use them under `#[allow(clippy::unwrap_used)]`).
- The endpoint contract is fixed (Slice 2a, merged): `GET /api/v1/designer/guardrail-policy?env={env}`, gtc_live_ bearer (tenant implied by token), `200 { "guardrails": [ { "cap_id", "config" } ] }` snake_case. `GuardrailRef` = `{ cap_id, offer_id: Option (serde default), config: Value (serde default) }`.
- Cache: 60s TTL; serve last-known on fetch error (any age); block (`Err`) only with no cached entry. Never hold a `Mutex` lock across an `.await`.
- Env-unset fallback: `StaticGuardrailPolicy(Vec::new())` (today's behavior).
- `bash ci/local_check.sh` is the gate (fmt + clippy `-D warnings` + test). English only; Conventional Commits; no AI attribution.
- Work in the worktree `greentic-runner/.claude/worktrees/guardrail-runtime` on branch `feat/guardrail-policy-provider`. Run all git from inside the worktree; confirm `git rev-parse --abbrev-ref HEAD` == `feat/guardrail-policy-provider` after each commit.

---

## File Structure

- `crates/greentic-aw-runtime/src/guardrail.rs` — `GuardrailPolicyError` + async trait + the two impls + their unit tests (Task 1).
- `crates/greentic-aw-runtime/src/loop.rs` — consumer awaits + maps `Err` → `GuardrailDenied` (Task 1).
- `crates/greentic-aw-runtime/src/guardrail_provider.rs` — new `HttpGuardrailPolicy` + cache + wiremock tests (Task 2).
- `crates/greentic-aw-runtime/src/lib.rs` — `pub mod guardrail_provider;` + re-export (Task 2).
- `crates/greentic-runner-host/src/runner/agent_node.rs` — `guardrail_policy_from_env()` + build-site wiring; remove `mandatory_guardrails_from_config()` (Task 3).

---

### Task 1: Make `GuardrailPolicy` async + fallible

**Files:**
- Modify: `crates/greentic-aw-runtime/src/guardrail.rs` (trait ~72-73; `NoMandatoryGuardrails` ~161-163; `StaticGuardrailPolicy` ~167-172; unit tests ~355-371)
- Modify: `crates/greentic-aw-runtime/src/loop.rs:61` (consumer)
- Modify (compile-fix `.await`): `crates/greentic-aw-runtime/tests/guardrail_e2e.rs`, `crates/greentic-aw-runtime/tests/guardrail_loop.rs` only if they call `mandatory_guardrails` directly (they construct `StaticGuardrailPolicy` and pass to `with_guardrails`; the call happens inside the loop, so likely no change — verify by compiling).

**Interfaces:**
- Produces: `pub enum GuardrailPolicyError { Unavailable(String) }` (derives `Debug` + `thiserror::Error`); the trait method becomes `fn mandatory_guardrails<'a>(&'a self, tenant: &'a TenantContext) -> Pin<Box<dyn Future<Output = Result<Vec<GuardrailRef>, GuardrailPolicyError>> + Send + 'a>>` (object-safe async, `Pin<Box<dyn Future>>` like `ConfigProvider`). `StaticGuardrailPolicy` and `NoMandatoryGuardrails` return `Box::pin(async move { Ok(...) })`.

- [ ] **Step 1: Write/adapt the failing test**

In `crates/greentic-aw-runtime/src/guardrail.rs` tests, add a fail-closed test and adapt the two existing ones to the async+Result signature:

```rust
#[tokio::test]
async fn static_policy_returns_refs() {
    let t = TenantContext::new("t", "e");
    let refs = vec![GuardrailRef {
        cap_id: "greentic:guardrail/pii".into(),
        offer_id: None,
        config: serde_json::Value::Null,
    }];
    let policy = StaticGuardrailPolicy(refs.clone());
    assert_eq!(policy.mandatory_guardrails(&t).await.unwrap(), refs);
}

#[tokio::test]
async fn no_mandatory_policy_is_empty() {
    let t = TenantContext::new("t", "e");
    assert!(NoMandatoryGuardrails
        .mandatory_guardrails(&t)
        .await
        .unwrap()
        .is_empty());
}
```

(Replace the existing sync `assert!(policy.mandatory_guardrails(&t).is_empty())` / `assert_eq!(...)` at lines ~357/370 with the `.await.unwrap()` forms above.)

- [ ] **Step 2: Run to verify it fails (compile error)**

Run: `cargo test -p greentic-aw-runtime --lib guardrail::`
Expected: FAIL to compile — trait is still sync; `.await` on a non-future.

- [ ] **Step 3: Implement the async trait + impls + error type**

In `crates/greentic-aw-runtime/src/guardrail.rs`, add the error type near the trait:

```rust
/// Failure obtaining the mandatory guardrail policy. Treated as fail-closed by
/// the agent loop (the step is denied), matching the unresolvable-mandatory-cap
/// behavior.
#[derive(Debug, thiserror::Error)]
pub enum GuardrailPolicyError {
    #[error("mandatory guardrail policy unavailable: {0}")]
    Unavailable(String),
}
```

Change the trait to an object-safe async method (`Pin<Box<dyn Future>>`, NOT `#[async_trait]` — the crate's `Cargo.toml` forbids async-trait on AW traits; this mirrors the `ConfigProvider` trait in `http_provider.rs`). Add the imports `use std::future::Future;` and `use std::pin::Pin;` at the top of `guardrail.rs` if absent:

```rust
pub trait GuardrailPolicy: Send + Sync {
    /// The platform-mandated guardrails for this tenant+env. `Err` means the
    /// policy could not be determined and the caller MUST fail closed.
    fn mandatory_guardrails<'a>(
        &'a self,
        tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<GuardrailRef>, GuardrailPolicyError>> + Send + 'a>>;
}
```

Update the two impls:

```rust
impl GuardrailPolicy for NoMandatoryGuardrails {
    fn mandatory_guardrails<'a>(
        &'a self,
        _tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<GuardrailRef>, GuardrailPolicyError>> + Send + 'a>> {
        Box::pin(async move { Ok(Vec::new()) })
    }
}

impl GuardrailPolicy for StaticGuardrailPolicy {
    fn mandatory_guardrails<'a>(
        &'a self,
        _tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<GuardrailRef>, GuardrailPolicyError>> + Send + 'a>> {
        let refs = self.0.clone();
        Box::pin(async move { Ok(refs) })
    }
}
```

Confirm `thiserror` is a workspace dep of `greentic-aw-runtime` (it is — see `Cargo.toml:48`). Do NOT add/use `async-trait` here.

- [ ] **Step 4: Update the consumer in `loop.rs:61`**

Replace the sync call with an awaited, fail-closed version:

```rust
let mandatory = match runtime.guardrail_policy.mandatory_guardrails(&tenant).await {
    Ok(m) => m,
    Err(e) => {
        warn!(error = %e, "mandatory guardrail policy unavailable; failing closed");
        return Err(AgentError::GuardrailDenied {
            direction: crate::guardrail::GuardrailDirection::Inbound,
            code: "internal".to_string(),
            message: "A required guardrail is unavailable.".to_string(),
            details: serde_json::to_string(
                &serde_json::json!({ "policy_unavailable": true }),
            )
            .ok(),
        });
    }
};
```

Leave the subsequent `assemble_chain(&registry, &mandatory, &config.guardrails)` block unchanged.

- [ ] **Step 5: Add a loop-level fail-closed test**

In `crates/greentic-aw-runtime/tests/guardrail_loop.rs` (or a new focused test module), add a policy stub that errors and assert the loop denies:

```rust
struct FailingPolicy;
impl greentic_aw_runtime::guardrail::GuardrailPolicy for FailingPolicy {
    fn mandatory_guardrails<'a>(
        &'a self,
        _t: &'a greentic_aw_runtime::tenant::TenantContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<
        Output = Result<
            Vec<greentic_aw_runtime::config::GuardrailRef>,
            greentic_aw_runtime::guardrail::GuardrailPolicyError,
        >,
    > + Send + 'a>> {
        Box::pin(async move {
            Err(greentic_aw_runtime::guardrail::GuardrailPolicyError::Unavailable("admin down".into()))
        })
    }
}
```

Wire it via `.with_guardrails(Arc::new(FailingPolicy), <evaluator>)` exactly as the existing `guardrail_loop.rs` test builds its runtime, run one step, and assert the result is `AgentError::GuardrailDenied { code, .. }` with `code == "internal"` (mirror how the existing test inspects the denied error). If the existing test harness is hard to reuse, assert at minimum that a step with `FailingPolicy` returns a `GuardrailDenied` error.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime --lib guardrail::` then `cargo test -p greentic-aw-runtime --test guardrail_loop --test guardrail_e2e`
Expected: PASS (fix any `.await` compile errors the async migration surfaces in those test files).

- [ ] **Step 7: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail.rs crates/greentic-aw-runtime/src/loop.rs crates/greentic-aw-runtime/tests/guardrail_loop.rs
git commit -m "feat: make GuardrailPolicy async + fallible (fail-closed on Err)"
```

---

### Task 2: `HttpGuardrailPolicy` provider

**Files:**
- Create: `crates/greentic-aw-runtime/src/guardrail_provider.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` (add `pub mod guardrail_provider;`)

**Interfaces:**
- Consumes: the async `GuardrailPolicy` trait + `GuardrailPolicyError` (Task 1); `GuardrailRef` (`crate::config`).
- Produces: `pub struct HttpGuardrailPolicy` with `pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self` (60s TTL) and `pub fn with_ttl(base_url, token, ttl: Duration) -> Self`. Implements `GuardrailPolicy`.

- [ ] **Step 1: Write the failing wiremock tests**

Create `crates/greentic-aw-runtime/src/guardrail_provider.rs` with a test module mirroring `http_provider.rs`:

```rust
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tenant::TenantContext;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn body(caps: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "guardrails": caps.iter().map(|c| serde_json::json!({
                "cap_id": c, "config": {}
            })).collect::<Vec<_>>()
        })
    }

    #[tokio::test]
    async fn fetches_resolved_guardrails() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/designer/guardrail-policy"))
            .and(query_param("env", "prod"))
            .and(header("authorization", "Bearer gtc_live_x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body(&["greentic:guardrail/pii"])))
            .mount(&server)
            .await;
        let p = HttpGuardrailPolicy::new(server.uri(), "gtc_live_x");
        let t = TenantContext::new("acme", "prod");
        let got = p.mandatory_guardrails(&t).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cap_id, "greentic:guardrail/pii");
    }

    #[tokio::test]
    async fn empty_policy_is_ok_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body(&[])))
            .mount(&server)
            .await;
        let p = HttpGuardrailPolicy::new(server.uri(), "gtc_live_x");
        let t = TenantContext::new("acme", "prod");
        assert!(p.mandatory_guardrails(&t).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn cache_hit_skips_second_http() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body(&["greentic:guardrail/pii"])))
            .expect(1) // exactly one upstream hit
            .mount(&server)
            .await;
        let p = HttpGuardrailPolicy::new(server.uri(), "gtc_live_x");
        let t = TenantContext::new("acme", "prod");
        p.mandatory_guardrails(&t).await.unwrap();
        p.mandatory_guardrails(&t).await.unwrap();
        // server's .expect(1) is verified on drop
    }

    #[tokio::test]
    async fn cold_failure_is_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let p = HttpGuardrailPolicy::new(server.uri(), "gtc_live_x");
        let t = TenantContext::new("acme", "prod");
        assert!(matches!(
            p.mandatory_guardrails(&t).await,
            Err(GuardrailPolicyError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn transient_failure_serves_stale() {
        // First a 200 to warm the cache (ttl 0 so the second call re-fetches),
        // then 503 → must serve the stale value, not error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body(&["greentic:guardrail/pii"])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let p = HttpGuardrailPolicy::with_ttl(server.uri(), "gtc_live_x", std::time::Duration::from_secs(0));
        let t = TenantContext::new("acme", "prod");
        let first = p.mandatory_guardrails(&t).await.unwrap(); // warms cache
        assert_eq!(first.len(), 1);
        let stale = p.mandatory_guardrails(&t).await.unwrap(); // 503 → serve stale
        assert_eq!(stale, first);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-aw-runtime --lib guardrail_provider`
Expected: FAIL to compile — `HttpGuardrailPolicy` does not exist.

- [ ] **Step 3: Implement `HttpGuardrailPolicy`**

In `crates/greentic-aw-runtime/src/guardrail_provider.rs` (above the test module). NOTE the cache discipline: hold the `Mutex` only to read/clone or to insert — never across the `.await`.

```rust
//! [`GuardrailPolicy`] that fetches the resolved mandatory guardrail list from
//! greentic-admin (`GET /api/v1/designer/guardrail-policy?env=`) per tenant+env,
//! with a 60s TTL cache. Serves the last-known list on transient admin failure
//! (fail-safe); returns `Unavailable` only when there is no cached entry (cold).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::GuardrailRef;
use crate::guardrail::{GuardrailPolicy, GuardrailPolicyError};
use crate::tenant::TenantContext;

const DEFAULT_TTL: Duration = Duration::from_secs(60);

#[derive(serde::Deserialize)]
struct PolicyResp {
    guardrails: Vec<GuardrailRef>,
}

pub struct HttpGuardrailPolicy {
    base_url: String,
    token: String,
    client: reqwest::Client,
    ttl: Duration,
    cache: Mutex<HashMap<(String, String), (Instant, Vec<GuardrailRef>)>>,
}

impl HttpGuardrailPolicy {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_ttl(base_url, token, DEFAULT_TTL)
    }

    pub fn with_ttl(base_url: impl Into<String>, token: impl Into<String>, ttl: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client,
            ttl,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn cached_fresh(&self, key: &(String, String)) -> Option<Vec<GuardrailRef>> {
        let guard = self.cache.lock().ok()?;
        let (at, refs) = guard.get(key)?;
        (at.elapsed() < self.ttl).then(|| refs.clone())
    }

    fn cached_any(&self, key: &(String, String)) -> Option<Vec<GuardrailRef>> {
        let guard = self.cache.lock().ok()?;
        guard.get(key).map(|(_, refs)| refs.clone())
    }

    fn store(&self, key: (String, String), refs: Vec<GuardrailRef>) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(key, (Instant::now(), refs));
        }
    }

    async fn fetch(&self, env: &str) -> Result<Vec<GuardrailRef>, String> {
        let url = format!("{}/api/v1/designer/guardrail-policy", self.base_url);
        let resp = self
            .client
            .get(&url)
            .query(&[("env", env)])
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        match resp.status().as_u16() {
            200 => resp
                .json::<PolicyResp>()
                .await
                .map(|p| p.guardrails)
                .map_err(|e| format!("decode: {e}")),
            other => Err(format!("status {other}")),
        }
    }
}

impl GuardrailPolicy for HttpGuardrailPolicy {
    fn mandatory_guardrails<'a>(
        &'a self,
        tenant: &'a TenantContext,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<GuardrailRef>, GuardrailPolicyError>> + Send + 'a>> {
        Box::pin(async move {
            let key = (tenant.tenant_id.clone(), tenant.env_id.clone());
            if let Some(fresh) = self.cached_fresh(&key) {
                return Ok(fresh);
            }
            match self.fetch(&key.1).await {
                Ok(refs) => {
                    self.store(key, refs.clone());
                    Ok(refs)
                }
                Err(reason) => match self.cached_any(&key) {
                    Some(stale) => {
                        tracing::warn!(error = %reason, "guardrail policy fetch failed; serving stale");
                        Ok(stale)
                    }
                    None => Err(GuardrailPolicyError::Unavailable(reason)),
                },
            }
        })
    }
}
```

Confirm `tenant.tenant_id` / `tenant.env_id` are the public field names on `TenantContext` (per `src/tenant.rs`); adjust if accessors differ. Add `pub mod guardrail_provider;` to `crates/greentic-aw-runtime/src/lib.rs` (near the other `pub mod` lines) and, if the crate re-exports key types at the root, add `pub use guardrail_provider::HttpGuardrailPolicy;`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime --lib guardrail_provider`
Expected: PASS (all five tests).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/guardrail_provider.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat: HttpGuardrailPolicy with TTL cache + serve-stale"
```

---

### Task 3: Wire the provider at the runner-host build site

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/agent_node.rs` (remove `mandatory_guardrails_from_config()` ~138-146; add `guardrail_policy_from_env()`; change the `.with_guardrails(...)` call ~726-730)

**Interfaces:**
- Consumes: `HttpGuardrailPolicy::new` (Task 2); `StaticGuardrailPolicy` (Task 1).
- Produces: `guardrail_policy_from_env() -> Option<HttpGuardrailPolicy>` (Some when both `GREENTIC_AW_ADMIN_ENDPOINT` and `GREENTIC_AW_ADMIN_TOKEN` are set+non-empty).

- [ ] **Step 1: Write the failing test**

Add a unit test next to the existing `agent_node.rs` tests (which already manipulate `GREENTIC_AW_ADMIN_ENDPOINT`/`_TOKEN` — see the env-var set/remove at ~1192-1225):

```rust
#[test]
fn guardrail_policy_from_env_requires_both_vars() {
    // Mirror the existing env-var test hygiene in this module (serialize via the
    // same guard the registry tests use; set/remove both vars).
    std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
    std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
    assert!(guardrail_policy_from_env().is_none());

    std::env::set_var("GREENTIC_AW_ADMIN_ENDPOINT", "http://localhost:9999");
    assert!(guardrail_policy_from_env().is_none()); // token missing

    std::env::set_var("GREENTIC_AW_ADMIN_TOKEN", "gtc_live_x");
    assert!(guardrail_policy_from_env().is_some());

    std::env::remove_var("GREENTIC_AW_ADMIN_ENDPOINT");
    std::env::remove_var("GREENTIC_AW_ADMIN_TOKEN");
}
```

(Use the exact env-var test guard/serialization the existing `registry_from_env`-style tests in this file use — they already set/remove these two vars at ~1192-1225, so follow that pattern to avoid cross-test races.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-runner-host guardrail_policy_from_env`
Expected: FAIL to compile — `guardrail_policy_from_env` does not exist.

- [ ] **Step 3: Implement the helper + wire the build site**

Add the helper near `registry_from_env()` (~294-310). Mirror its env-read shape:

```rust
/// Build an [`HttpGuardrailPolicy`] from `GREENTIC_AW_ADMIN_ENDPOINT` +
/// `GREENTIC_AW_ADMIN_TOKEN` (the same pair the agent registry uses). Returns
/// `None` when either is unset/empty, so a non-admin deploy enforces no
/// mandatory policy (today's behavior).
fn guardrail_policy_from_env() -> Option<greentic_aw_runtime::guardrail_provider::HttpGuardrailPolicy> {
    let endpoint = std::env::var("GREENTIC_AW_ADMIN_ENDPOINT").ok().filter(|s| !s.is_empty())?;
    let token = std::env::var("GREENTIC_AW_ADMIN_TOKEN").ok().filter(|s| !s.is_empty())?;
    Some(greentic_aw_runtime::guardrail_provider::HttpGuardrailPolicy::new(endpoint, token))
}
```

Remove the `mandatory_guardrails_from_config()` stub (~138-146). Change the `.with_guardrails(...)` call (~726-730) from:

```rust
.with_guardrails(
    Arc::new(greentic_aw_runtime::guardrail::StaticGuardrailPolicy(
        mandatory_guardrails_from_config(),
    )),
    Arc::new(greentic_aw_runtime::guardrail::ExtRuntimeGuardrailEvaluator {
        ext_runtime: ext_runtime.clone(),
    }),
);
```

to:

```rust
.with_guardrails(
    {
        let policy: Arc<dyn greentic_aw_runtime::guardrail::GuardrailPolicy> =
            match guardrail_policy_from_env() {
                Some(http) => Arc::new(http),
                None => Arc::new(greentic_aw_runtime::guardrail::StaticGuardrailPolicy(Vec::new())),
            };
        policy
    },
    Arc::new(greentic_aw_runtime::guardrail::ExtRuntimeGuardrailEvaluator {
        ext_runtime: ext_runtime.clone(),
    }),
);
```

Apply the same change at every build site that currently passes `StaticGuardrailPolicy(mandatory_guardrails_from_config())` (grep `mandatory_guardrails_from_config` across `crates/greentic-runner-host` to confirm there is only the one site; if there are more, update each).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-runner-host guardrail_policy_from_env` then `cargo build -p greentic-runner-host`
Expected: PASS + clean build (no remaining reference to `mandatory_guardrails_from_config`).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/runner/agent_node.rs
git commit -m "feat: wire HttpGuardrailPolicy from admin env, static-empty fallback"
```

---

### Task 4: Workspace verification

- [ ] **Step 1: Grep for any missed sites**

Run: `git grep -n 'mandatory_guardrails_from_config'`
Expected: no matches (stub fully removed).

Run: `git grep -nE 'mandatory_guardrails\(' -- 'crates/**/*.rs' | grep -v '\.await'`
Expected: only the trait/impl definitions (no remaining sync call sites).

- [ ] **Step 2: Targeted tests (disk-safe — do NOT build the whole workspace at once if disk is tight)**

Run: `cargo test -p greentic-aw-runtime guardrail` then `cargo test -p greentic-runner-host guardrail`
Expected: all guardrail unit + provider + loop/e2e tests PASS.

- [ ] **Step 3: fmt + clippy on the touched crates**

Run: `cargo fmt -p greentic-aw-runtime -p greentic-runner-host -- --check` and `cargo clippy -p greentic-aw-runtime -p greentic-runner-host --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Optional full gate (only if disk headroom allows)**

Run: `bash ci/local_check.sh`
If it fails on disk/linker (environment), document it and rely on the targeted runs + the PR CI instead — note this in the PR description.

- [ ] **Step 5: Version bump note**

If `crates/greentic-aw-runtime/Cargo.toml` carries a semver used by downstream pins, bump the minor (the `GuardrailPolicy` trait change is breaking for external impls). Record in the commit. (Designer re-pin is a separate downstream PR per the spec §8.)

```bash
git add -A && git commit -m "chore: bump greentic-aw-runtime for async GuardrailPolicy" # only if a version field exists
```

---

## Self-Review

**Spec coverage:**
- §4.1 trait async+fallible + error type → Task 1.
- §4.2 consumer maps Err → GuardrailDenied → Task 1 (Step 4) + fail-closed test (Step 5).
- §4.3 HttpGuardrailPolicy + cache + serve-stale → Task 2.
- §4.4 build-site wiring + env fallback → Task 3.
- §5 edge cases (empty→Ok-empty, cold→Err, transient→stale, malformed→fetch-fail) → Task 2 tests (empty_policy_is_ok_empty, cold_failure_is_unavailable, transient_failure_serves_stale; malformed-body falls into the non-200/decode→Err path covered by the fetch impl).
- §6 testing → Tasks 1-3 tests + Task 4.
- §7 constraints (no unwrap in prod, lock-not-across-await, contract shape) → enforced in Task 2 impl + Global Constraints.

**Gap noted:** §6 lists a `malformed_body_cold_is_unavailable` test; the fetch impl maps a decode error to `Err(reason)` → cold → `Unavailable`, but no explicit test asserts it. Add one in Task 2 if cheap:

```rust
#[tokio::test]
async fn malformed_body_cold_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .mount(&server).await;
    let p = HttpGuardrailPolicy::new(server.uri(), "gtc_live_x");
    let t = TenantContext::new("acme", "prod");
    assert!(matches!(p.mandatory_guardrails(&t).await, Err(GuardrailPolicyError::Unavailable(_))));
}
```

**Placeholder scan:** no TBD/TODO-as-spec; every code step has concrete code. The "confirm field names / deps already present" notes are real verification instructions, not placeholders.

**Type consistency:** `GuardrailPolicyError::Unavailable(String)`, the object-safe `fn mandatory_guardrails<'a>(&'a self, &'a TenantContext) -> Pin<Box<dyn Future<Output = Result<Vec<GuardrailRef>, GuardrailPolicyError>> + Send + 'a>>` (NO `#[async_trait]` — crate convention), `HttpGuardrailPolicy::{new, with_ttl}`, `guardrail_policy_from_env() -> Option<HttpGuardrailPolicy>` — consistent across Tasks 1-3. Response wrapper `PolicyResp { guardrails: Vec<GuardrailRef> }` matches the Slice 2a `{guardrails:[...]}` contract. `tenant.tenant_id`/`tenant.env_id` field access flagged for verification in Task 2 Step 3.
