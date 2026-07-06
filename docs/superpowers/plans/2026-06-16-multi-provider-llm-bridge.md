# Multi-provider LLM bridge — Implementation Plan (sub-project 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A digital worker's chosen LLM provider (from the designer dropdown) runs end-to-end at runtime, per-tenant, for 9 providers, via a WASM LLM-bridge extension that the runner invokes with per-tenant credentials resolved from the secrets broker.

**Architecture:** Arch A (LLM-as-extension). The runner resolves a per-tenant API key from the `greentic-secrets-broker` using a credential ref carried in the worker config, passes it to a new `greentic-llm-bridge` WASM extension's `complete` tool, which hand-rolls per-provider HTTP via the host `extension-host/http` import and returns a normalized `LlmResponse`. The 9 providers collapse to 4 mapper families.

**Tech Stack:** Rust (edition 2024, runner pinned 1.94 / extensions 1.95), `cargo component` → `wasm32-wasip2`, `wit-bindgen`, `reqwest` (runner only), `greentic-secrets-broker` HTTP, `greentic-ext-runtime`, axum (broker).

**Spec:** `greentic-runner/docs/superpowers/specs/2026-06-16-multi-provider-llm-bridge-design.md`

**Scope note:** This plan spans 4 subsystems (runner, bridge extension, designer/catalog, deploy). It is sequenced as one e2e but each phase produces independently testable software. If you prefer, Phases 1–2 (runner), Phase 3 (bridge), Phase 4–5 (designer/catalog), Phase 6 (deploy) can each be executed as a sub-plan.

**Canonical facts (verified from source — do not re-derive):**
- Wire types live in `greentic-runner/crates/greentic-aw-runtime/src/{llm.rs,state.rs,config.rs,error.rs}`.
- `ChatMessage` is `#[serde(tag = "role", rename_all = "snake_case")]` with variants `System{content}`, `User{content}`, `Assistant{content, tool_calls}`, `Tool{call_id, content: Value}`.
- `ToolCallRecord { call_id, extension_id, tool_name, args: Value }`.
- `LlmResponse { content: Option<String>, tool_calls: Vec<ToolCallRecord>, tokens_in: u32, tokens_out: u32 }`.
- `BridgeCredential { provider, model, api_key, base_url: Option<String> }` and `BridgeRequest { system_prompt, history, tools, credential }` already exist in `greentic-aw-runtime/src/llm_extension.rs`.
- Runtime tool contract: guest exports `greentic:extension-design/tools@0.2.0` fn `invoke-tool(name, args-json) -> result<string, extension-error>`; tool declared in `describe.json` `contributions.tools[]` as `{ "name": "complete", "export": "greentic:extension-design/tools.invoke-tool" }`.
- Admin writes the LLM key at URI `secrets://default/{tenant}/_/llm/{provider_uuid}` (`greentic-designer-admin/src/routes/admin/tenant_llm.rs`), where the name segment is the **provider row UUID**.
- Broker GET route: `GET /v1/{env}/{tenant}/{team}/{category}/{name}` (and no-team variant) → JSON `SecretResponse { value: String, encoding: "utf8"|"base64", ... }`, Bearer auth. No reusable Rust broker client exists — build one.
- `SecretsManager` trait (`greentic-secrets-api`): `async fn read(&self, path:&str)->Result<Vec<u8>>; write; delete`. Error `SecretError`.
- `AgentRuntime` is constructed **per-tenant** in `TenantRuntime::from_packs` via `build_agent_node_handler(merged_agents)`. `TenantRuntime` already holds `tenant: String` and `secrets: DynSecretsManager`.

**Provider registry (9 providers, 4 families):**

| slug | family | base_url | default model | auth |
|---|---|---|---|---|
| openai | openai | https://api.openai.com | gpt-4o-mini | `Authorization: Bearer` |
| deepseek | openai | https://api.deepseek.com | deepseek-chat | Bearer |
| groq | openai | https://api.groq.com/openai | llama-3.3-70b-versatile | Bearer |
| perplexity | openai | https://api.perplexity.ai | sonar | Bearer |
| xai | openai | https://api.x.ai | grok-2 | Bearer |
| ollama | openai | http://localhost:11434 | llama3.1 | Bearer (optional) |
| openai-compatible | openai | (from base_url override) | (model from config) | Bearer |
| anthropic | anthropic | https://api.anthropic.com | claude-3-5-sonnet-latest | `x-api-key` + `anthropic-version: 2023-06-01` |
| gemini | gemini | https://generativelanguage.googleapis.com | gemini-1.5-flash | `?key=` query param |
| cohere | cohere | https://api.cohere.com | command-r-plus | Bearer |

`base_url` is overridable per request via `credential.base_url`. `model` always comes from `request.provider.model`; the default column is only the catalog default.

---

## Phase 0 — De-risk the broker URI round-trip

**Why first:** the runner must read the EXACT key the admin writes. The admin scope `team=None` renders as `_`; confirm the broker stores/serves it under the same path the runner will GET. This phase proves the contract before building on it.

### Task 0.1: Round-trip integration test (admin write → broker → raw GET)

**Files:**
- Create: `greentic-runner/crates/greentic-runner-host/tests/broker_uri_roundtrip.rs`

- [ ] **Step 1: Write the failing integration test**

```rust
//! Verifies the runner reads the IDENTICAL broker key the admin writes for an
//! LLM credential. Requires a running broker; skipped when SECRETS_BROKER_ENDPOINT unset.
use std::env;

#[tokio::test]
async fn admin_written_llm_key_is_readable_at_runner_uri() {
    let Some(endpoint) = env::var("SECRETS_BROKER_ENDPOINT").ok() else {
        eprintln!("SECRETS_BROKER_ENDPOINT unset; skipping");
        return;
    };
    let token = env::var("SECRETS_BROKER_TOKEN").unwrap_or_default();
    let client = reqwest::Client::new();
    let (tenant, provider_uuid) = ("t-roundtrip", "11111111-1111-1111-1111-111111111111");

    // 1. Simulate the admin write: PUT /v1/default/{tenant}/_/llm/{uuid}
    let put = client
        .put(format!("{endpoint}/v1/default/{tenant}/_/llm/{provider_uuid}"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "visibility": "private", "content_type": "text", "encoding": "utf8",
            "value": "sk-roundtrip-secret"
        }))
        .send().await.expect("put");
    assert!(put.status().is_success(), "put status {}", put.status());

    // 2. Read it back exactly as the runner's BrokerSecretsManager will.
    let got = client
        .get(format!("{endpoint}/v1/default/{tenant}/_/llm/{provider_uuid}"))
        .bearer_auth(&token)
        .send().await.expect("get");
    assert!(got.status().is_success(), "get status {}", got.status());
    let body: serde_json::Value = got.json().await.expect("json");
    assert_eq!(body["value"], "sk-roundtrip-secret");
}
```

- [ ] **Step 2: Run it (with a local broker) to verify it passes or reveals the path mismatch**

Run:
```bash
cd greentic-runner
# Start broker in another shell from greentic-secrets/greentic-secrets-broker (file backend), export SECRETS_BROKER_ENDPOINT/TOKEN
cargo test -p greentic-runner-host --test broker_uri_roundtrip -- --nocapture
```
Expected: PASS. If GET 404s on the `_` team segment, inspect `greentic-secrets-broker/src/http.rs` route handling of the `team="_"` segment and adjust the runner's path builder in Task 1.2 accordingly (record the working path shape in a comment).

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-runner-host/tests/broker_uri_roundtrip.rs
git commit -m "test: pin broker URI round-trip contract for LLM credentials"
```

---

## Phase 1 — Runner: broker-backed SecretsManager

### Task 1.1: `BrokerSecretsManager` parses a `secrets://` URI to a broker path

**Files:**
- Create: `greentic-runner/crates/greentic-runner-host/src/secrets_broker.rs`
- Modify: `greentic-runner/crates/greentic-runner-host/src/secrets.rs` (add `mod` + re-export)
- Test: inline `#[cfg(test)]` in `secrets_broker.rs`

- [ ] **Step 1: Write the failing test for URI→path mapping**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_secrets_uri_to_broker_v1_path() {
        // secrets://default/acme/_/llm/uuid  ->  /v1/default/acme/_/llm/uuid
        let p = broker_path_from_uri("secrets://default/acme/_/llm/abc-123").unwrap();
        assert_eq!(p, "/v1/default/acme/_/llm/abc-123");
    }

    #[test]
    fn rejects_non_secrets_uri() {
        assert!(broker_path_from_uri("https://x/y").is_err());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-runner-host secrets_broker -- --nocapture`
Expected: FAIL — `broker_path_from_uri` not found.

- [ ] **Step 3: Implement the module skeleton + path mapper**

```rust
//! `BrokerSecretsManager` — reads/writes per-tenant secrets over the
//! greentic-secrets-broker HTTP API. The runner uses this to resolve
//! per-tenant LLM credentials written by greentic-designer-admin.

use async_trait::async_trait;
use base64::Engine as _;
use greentic_secrets_lib::{SecretError, SecretsManager};
use reqwest::Client;
use serde::Deserialize;

/// Convert a canonical `secrets://{env}/{tenant}/{team}/{cat}/{name}` URI into
/// the broker `/v1/...` request path. Path segments are preserved verbatim
/// (the broker treats `_` as the tenant-wide team).
pub(crate) fn broker_path_from_uri(uri: &str) -> Result<String, SecretError> {
    let rest = uri
        .strip_prefix("secrets://")
        .ok_or_else(|| SecretError::Backend("not a secrets:// uri".into()))?;
    if rest.is_empty() {
        return Err(SecretError::Backend("empty secrets uri".into()));
    }
    Ok(format!("/v1/{rest}"))
}

#[derive(Deserialize)]
struct SecretResponse {
    value: String,
    #[serde(default)]
    encoding: String, // "utf8" | "base64"
}

pub struct BrokerSecretsManager {
    client: Client,
    endpoint: String, // e.g. http://secrets-broker:8080
    token: String,
}

impl BrokerSecretsManager {
    pub fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }
}
```

- [ ] **Step 4: Run the path test to verify it passes**

Run: `cargo test -p greentic-runner-host secrets_broker -- --nocapture`
Expected: PASS (both URI tests).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/secrets_broker.rs crates/greentic-runner-host/src/secrets.rs
git commit -m "feat(runner): broker secrets URI path mapper"
```

### Task 1.2: Implement `SecretsManager` over HTTP

**Files:**
- Modify: `greentic-runner/crates/greentic-runner-host/src/secrets_broker.rs`

- [ ] **Step 1: Write the failing test (mock server) for `read` decoding**

```rust
    #[tokio::test]
    async fn read_decodes_utf8_value() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/default/acme/_/llm/abc"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "uri":"secrets://default/acme/_/llm/abc","version":1,"visibility":"private",
                "content_type":"text","encoding":"utf8","value":"sk-xyz"
            })))
            .mount(&server).await;
        let mgr = BrokerSecretsManager::new(server.uri(), "tok");
        let bytes = mgr.read("secrets://default/acme/_/llm/abc").await.unwrap();
        assert_eq!(bytes, b"sk-xyz");
    }
```

Add `wiremock` to `[dev-dependencies]` in `crates/greentic-runner-host/Cargo.toml` if absent.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-runner-host secrets_broker::tests::read_decodes -- --nocapture`
Expected: FAIL — `SecretsManager` not implemented for `BrokerSecretsManager`.

- [ ] **Step 3: Implement the trait**

```rust
#[async_trait]
impl SecretsManager for BrokerSecretsManager {
    async fn read(&self, path: &str) -> greentic_secrets_lib::Result<Vec<u8>> {
        let url = format!("{}{}", self.endpoint, broker_path_from_uri(path)?);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SecretError::Backend(e.to_string().into()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SecretError::NotFound(path.to_string()));
        }
        if resp.status() == reqwest::StatusCode::FORBIDDEN
            || resp.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(SecretError::Permission(path.to_string()));
        }
        if !resp.status().is_success() {
            return Err(SecretError::Backend(format!("broker {}", resp.status()).into()));
        }
        let body: SecretResponse = resp
            .json()
            .await
            .map_err(|e| SecretError::Backend(e.to_string().into()))?;
        match body.encoding.as_str() {
            "base64" => base64::engine::general_purpose::STANDARD
                .decode(body.value.as_bytes())
                .map_err(|e| SecretError::Backend(e.to_string().into())),
            _ => Ok(body.value.into_bytes()),
        }
    }

    async fn write(&self, path: &str, bytes: &[u8]) -> greentic_secrets_lib::Result<()> {
        let url = format!("{}{}", self.endpoint, broker_path_from_uri(path)?);
        let value = String::from_utf8(bytes.to_vec())
            .map_err(|e| SecretError::Backend(e.to_string().into()))?;
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "visibility":"private","content_type":"text","encoding":"utf8","value":value
            }))
            .send()
            .await
            .map_err(|e| SecretError::Backend(e.to_string().into()))?;
        if !resp.status().is_success() {
            return Err(SecretError::Backend(format!("broker put {}", resp.status()).into()));
        }
        Ok(())
    }

    async fn delete(&self, path: &str) -> greentic_secrets_lib::Result<()> {
        let url = format!("{}{}", self.endpoint, broker_path_from_uri(path)?);
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| SecretError::Backend(e.to_string().into()))?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(SecretError::Backend(format!("broker delete {}", resp.status()).into()));
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p greentic-runner-host secrets_broker -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/secrets_broker.rs crates/greentic-runner-host/Cargo.toml
git commit -m "feat(runner): BrokerSecretsManager over greentic-secrets-broker HTTP"
```

### Task 1.3: Wire `SecretsBackend::Broker`

**Files:**
- Modify: `greentic-runner/crates/greentic-runner-host/src/secrets.rs:18-52` (enum + `from_env` + `build_manager`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn from_env_parses_broker() {
        let b = SecretsBackend::from_env(Some("broker".into())).unwrap();
        assert!(matches!(b, SecretsBackend::Broker { .. }));
    }
```
(Set `SECRETS_BROKER_ENDPOINT`/`SECRETS_BROKER_TOKEN` in the test via `std::env::set_var` before the call, or have `from_env` read them lazily in `build_manager`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-runner-host secrets::` ; Expected: FAIL — no `Broker` variant.

- [ ] **Step 3: Extend the enum and constructors**

```rust
#[derive(Clone, Debug)]
pub enum SecretsBackend {
    Env,
    Broker { endpoint: String, token: String },
}

impl SecretsBackend {
    pub fn from_env(value: Option<String>) -> Result<Self> {
        match value.unwrap_or_else(|| "env".into()).trim().to_ascii_lowercase().as_str() {
            "" | "env" => Ok(SecretsBackend::Env),
            "broker" => Ok(SecretsBackend::Broker {
                endpoint: std::env::var("SECRETS_BROKER_ENDPOINT")
                    .map_err(|_| anyhow!("SECRETS_BACKEND=broker requires SECRETS_BROKER_ENDPOINT"))?,
                token: std::env::var("SECRETS_BROKER_TOKEN").unwrap_or_default(),
            }),
            other => Err(anyhow!("unsupported SECRETS_BACKEND `{other}`")),
        }
    }

    pub fn build_manager(&self) -> Result<DynSecretsManager> {
        let inner: DynSecretsManager = match self {
            SecretsBackend::Env => {
                ensure_env_secrets_allowed()?;
                Arc::new(EnvSecretsManager)
            }
            SecretsBackend::Broker { endpoint, token } => {
                Arc::new(crate::secrets_broker::BrokerSecretsManager::new(endpoint, token))
            }
        };
        Ok(CachingSecretsManager::wrap(inner))
    }
}
```
Also add the `Broker` arm to `from_config` (map `"broker"` → read endpoint/token from the config struct or env).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p greentic-runner-host secrets:: -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/secrets.rs
git commit -m "feat(runner): SecretsBackend::Broker variant"
```

---

## Phase 2 — Runner: per-tenant credential resolution

### Task 2.1: Add `credential_ref` to `LlmProviderRef`

**Files:**
- Modify: `greentic-runner/crates/greentic-aw-runtime/src/config.rs:20-24`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn llm_provider_ref_defaults_credential_ref_to_none() {
        let r: LlmProviderRef =
            serde_json::from_str(r#"{ "provider":"anthropic","model":"claude-3" }"#).unwrap();
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.credential_ref, None);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-aw-runtime llm_provider_ref_defaults -- --nocapture`
Expected: FAIL — no field `credential_ref`.

- [ ] **Step 3: Add the field (serde default = backward compatible)**

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProviderRef {
    pub provider: String, // "openai" | "anthropic" | ...
    pub model: String,    // "gpt-4o-mini" | "claude-3-haiku" | ...
    /// Per-tenant credential identifier (admin provider UUID) used by the
    /// runner to resolve `secrets://default/{tenant}/_/llm/{credential_ref}`.
    /// `None` falls back to env-keyed credentials (legacy single-provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
}
```

- [ ] **Step 4: Run to verify pass + the existing roundtrip test still passes**

Run: `cargo test -p greentic-aw-runtime config:: -- --nocapture`
Expected: PASS. (Existing `agent_config_roundtrips_through_json` still passes — field is optional.)

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/config.rs
git commit -m "feat(aw-runtime): LlmProviderRef.credential_ref for per-tenant creds"
```

### Task 2.2: `SecretsBackedCredentialResolver`

**Files:**
- Create: `greentic-runner/crates/greentic-aw-runtime/src/llm_credential.rs`
- Modify: `greentic-aw-runtime/src/lib.rs` (add `mod llm_credential;` + re-export)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LlmProviderRef;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct FakeSecrets;
    #[async_trait]
    impl greentic_secrets_lib::SecretsManager for FakeSecrets {
        async fn read(&self, path: &str) -> greentic_secrets_lib::Result<Vec<u8>> {
            assert_eq!(path, "secrets://default/acme/_/llm/cred-uuid");
            Ok(b"sk-live".to_vec())
        }
        async fn write(&self, _: &str, _: &[u8]) -> greentic_secrets_lib::Result<()> { Ok(()) }
        async fn delete(&self, _: &str) -> greentic_secrets_lib::Result<()> { Ok(()) }
    }

    #[tokio::test]
    async fn builds_bridge_credential_from_request_and_secret() {
        let resolver = SecretsBackedCredentialResolver::new(Arc::new(FakeSecrets), "acme");
        let pr = LlmProviderRef {
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet-latest".into(),
            credential_ref: Some("cred-uuid".into()),
        };
        let cred = resolver.resolve(&pr).await.unwrap();
        assert_eq!(cred.provider, "anthropic");
        assert_eq!(cred.model, "claude-3-5-sonnet-latest");
        assert_eq!(cred.api_key, "sk-live");
        assert_eq!(cred.base_url, None);
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-aw-runtime llm_credential -- --nocapture`
Expected: FAIL — module/type missing.

- [ ] **Step 3: Implement the resolver**

```rust
//! Resolves a per-tenant `BridgeCredential` from a worker's `LlmProviderRef`
//! and the tenant-scoped secrets manager. The secret URI matches what
//! greentic-designer-admin writes: `secrets://default/{tenant}/_/llm/{ref}`.

use std::sync::Arc;

use crate::config::LlmProviderRef;
use crate::error::LlmError;
use crate::llm_extension::BridgeCredential;
use greentic_secrets_lib::SecretsManager;

pub struct SecretsBackedCredentialResolver {
    secrets: Arc<dyn SecretsManager>,
    tenant: String,
}

impl SecretsBackedCredentialResolver {
    pub fn new(secrets: Arc<dyn SecretsManager>, tenant: impl Into<String>) -> Self {
        Self { secrets, tenant: tenant.into() }
    }

    pub async fn resolve(&self, pr: &LlmProviderRef) -> Result<BridgeCredential, LlmError> {
        let cref = pr.credential_ref.as_deref().ok_or_else(|| {
            LlmError::BadRequest(format!("no credential_ref for provider `{}`", pr.provider))
        })?;
        let uri = format!("secrets://default/{}/_/llm/{}", self.tenant, cref);
        let bytes = self
            .secrets
            .read(&uri)
            .await
            .map_err(|e| LlmError::BadRequest(format!("resolve credential: {e}")))?;
        let api_key = String::from_utf8(bytes)
            .map_err(|e| LlmError::BadRequest(format!("credential not utf8: {e}")))?;
        Ok(BridgeCredential {
            provider: pr.provider.clone(),
            model: pr.model.clone(),
            api_key,
            base_url: None,
        })
    }
}
```

Make `BridgeCredential` fields + the struct `pub` in `llm_extension.rs` if not already.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p greentic-aw-runtime llm_credential -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/llm_credential.rs crates/greentic-aw-runtime/src/lib.rs crates/greentic-aw-runtime/src/llm_extension.rs
git commit -m "feat(aw-runtime): per-tenant SecretsBackedCredentialResolver"
```

### Task 2.3: `ExtensionLlmBackend` resolves per request

**Files:**
- Modify: `greentic-aw-runtime/src/llm_extension.rs:66-130`

- [ ] **Step 1: Write the failing test** — a backend built with a resolver sends a credential derived from `request.provider`, not a frozen one.

```rust
    #[tokio::test]
    async fn resolving_backend_uses_request_provider_credential() {
        use crate::llm_credential::SecretsBackedCredentialResolver;
        use std::sync::Arc;
        // FakeSecrets returns a key for the cred-uuid path (see llm_credential tests).
        let resolver = Arc::new(SecretsBackedCredentialResolver::new(Arc::new(FakeSecrets), "acme"));
        let inv = Arc::new(ScriptInvoker {
            seen: std::sync::Mutex::new(None),
            reply: Ok(serde_json::json!({"content":"ok","tool_calls":[],"tokens_in":1,"tokens_out":1}).to_string()),
        });
        let backend = ExtensionLlmBackend::with_resolver(inv.clone(), "llm-bridge", resolver);
        let mut r = req();
        r.provider.provider = "anthropic".into();
        r.provider.credential_ref = Some("cred-uuid".into());
        backend.complete(r).await.unwrap();
        let (_ext, _tool, args) = inv.seen.lock().unwrap().clone().unwrap();
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["credential"]["provider"], "anthropic");
        assert_eq!(v["credential"]["api_key"], "sk-live");
    }
```
(Reuse `FakeSecrets` from Task 2.2 by making it `pub(crate)` test util, or duplicate it in this test module.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-aw-runtime resolving_backend -- --nocapture`
Expected: FAIL — `with_resolver` not found.

- [ ] **Step 3: Add a resolver-backed constructor + per-request resolution**

Add a credential source enum to the backend so both the legacy frozen path and the new resolver path coexist:

```rust
enum CredentialSource {
    Static(BridgeCredential),
    Resolver(std::sync::Arc<crate::llm_credential::SecretsBackedCredentialResolver>),
}

pub struct ExtensionLlmBackend {
    invoker: Arc<dyn LlmExtensionInvoker>,
    extension_id: String,
    credential: CredentialSource,
}

impl ExtensionLlmBackend {
    // keep existing `new(...)` building CredentialSource::Static(credential)
    pub fn with_resolver(
        invoker: Arc<dyn LlmExtensionInvoker>,
        extension_id: impl Into<String>,
        resolver: std::sync::Arc<crate::llm_credential::SecretsBackedCredentialResolver>,
    ) -> Self {
        Self { invoker, extension_id: extension_id.into(), credential: CredentialSource::Resolver(resolver) }
    }
    // also a prod ctor taking ext_runtime + resolver, mirroring `new`
}
```

In `complete`, resolve before building the payload:

```rust
let credential = match &self.credential {
    CredentialSource::Static(c) => c.clone(),
    CredentialSource::Resolver(r) => r.resolve(&request.provider).await?,
};
```
(everything else — `BridgeRequest`, `invoke`, decode — stays identical).

- [ ] **Step 4: Run to verify pass + existing extension tests still pass**

Run: `cargo test -p greentic-aw-runtime llm_extension -- --nocapture`
Expected: PASS (old `sends_complete_tool_with_credential_and_parses_response` still green via the Static path).

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/llm_extension.rs
git commit -m "feat(aw-runtime): ExtensionLlmBackend resolves credential per request"
```

### Task 2.4: Thread tenant + secrets into `build_agent_node_handler`

**Files:**
- Modify: `greentic-runner-host/src/runner/agent_node.rs:298-389` (signature + LLM construction)
- Modify: caller in `greentic-runner-host/src/runtime.rs:~237` (`TenantRuntime::from_packs`)

- [ ] **Step 1: Write the failing test** — handler builder accepts tenant + secrets and, when `GREENTIC_AW_LLM_EXTENSION` is set, wires the resolver path.

Add a focused unit test in `agent_node.rs`'s test module asserting `bridge_credential`/resolver selection given a fake `DynSecretsManager` and a non-empty `GREENTIC_AW_LLM_EXTENSION`. (Use the existing test scaffolding in that module; assert the constructed handler is `Some`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p greentic-runner-host agent_node -- --nocapture`
Expected: FAIL — arity mismatch.

- [ ] **Step 3: Change the signature + use the resolver when the bridge env is set**

```rust
pub async fn build_agent_node_handler(
    merged_agents: HashMap<String, AgentConfig>,
    tenant: String,
    secrets: crate::secrets::DynSecretsManager,
) -> Option<Arc<dyn AgentNodeHandler>> {
    // ... unchanged up to the llm block ...
    use greentic_aw_runtime::llm_credential::SecretsBackedCredentialResolver;

    let llm: Arc<dyn LlmBackend> = match std::env::var("GREENTIC_AW_LLM_EXTENSION")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(ext_id) => {
            let resolver = Arc::new(SecretsBackedCredentialResolver::new(secrets.clone(), tenant.clone()));
            tracing::info!(extension = %ext_id, tenant = %tenant, "AW LLM via bridge (per-tenant creds)");
            Arc::new(RetryingLlmBackend::new(
                ExtensionLlmBackend::with_resolver_runtime(ext_runtime.clone(), ext_id, resolver),
                3, Duration::from_millis(250),
            ))
        }
        None => {
            let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            Arc::new(RetryingLlmBackend::new(OpenAiLlmBackend::new(openai_key), 3, Duration::from_millis(250)))
        }
    };
    // ... unchanged ...
}
```
Add `ExtensionLlmBackend::with_resolver_runtime(ext_runtime, ext_id, resolver)` (prod variant of `with_resolver` that builds the `RuntimeInvoker` internally).

Update the caller:
```rust
if let Some(handler) = crate::runner::agent_node::build_agent_node_handler(
    merged_agents, self.tenant.clone(), self.secrets.clone(),
).await {
    engine.set_agent_node_handler(handler);
}
```

- [ ] **Step 4: Run full crate tests**

Run: `cargo test -p greentic-runner-host -- --nocapture` then `cargo clippy -p greentic-runner-host --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/runner/agent_node.rs crates/greentic-runner-host/src/runtime.rs crates/greentic-aw-runtime/src/llm_extension.rs
git commit -m "feat(runner): per-tenant LLM credential resolution wired into AW runtime"
```

---

## Phase 3 — `greentic-llm-bridge` WASM extension

Work in `greentic-llm-extensions` (branch off `research`). Template: `reference-extensions/llm-openai` and `components-public/crates/http-extension`.

### Task 3.1: Scaffold the extension crate + WIT + describe.json

**Files:**
- Create: `greentic-llm-extensions/reference-extensions/llm-bridge/Cargo.toml`
- Create: `.../llm-bridge/wit/world.wit`
- Create: `.../llm-bridge/describe.json`
- Create: `.../llm-bridge/src/lib.rs` (stub `invoke_tool`)
- Copy: `build.sh`, `.cargo/config.toml`, `wit/deps/` from `llm-openai`

- [ ] **Step 1: Write `wit/world.wit`**

```wit
package greentic:llm-bridge;

world extension {
  import greentic:extension-base/types@0.1.0;
  import greentic:extension-host/logging@0.1.0;
  import greentic:extension-host/http@0.1.0;

  export greentic:extension-base/manifest@0.1.0;
  export greentic:extension-base/lifecycle@0.1.0;
  export greentic:extension-design/tools@0.2.0;
}
```

- [ ] **Step 2: Write `describe.json`** (kind DesignExtension, tool `complete`, network allowlist for all 9 endpoints)

```json
{
  "$schema": "https://store.greentic.cloud/schemas/describe-v1.json",
  "apiVersion": "greentic.ai/v1",
  "kind": "DesignExtension",
  "metadata": { "id": "greentic.llm-bridge", "name": "LLM Bridge",
    "version": "1.2.0-research", "summary": "Runtime LLM provider bridge for digital workers" },
  "runtime": { "component": "extension.wasm", "memoryLimitMB": 64,
    "permissions": { "network": [
      "https://api.openai.com","https://api.deepseek.com","https://api.groq.com",
      "https://api.perplexity.ai","https://api.x.ai","https://api.anthropic.com",
      "https://generativelanguage.googleapis.com","https://api.cohere.com"
    ], "secrets": [], "callExtensionKinds": [] } },
  "contributions": { "tools": [
    { "name": "complete", "export": "greentic:extension-design/tools.invoke-tool" }
  ] }
}
```

- [ ] **Step 3: Stub `src/lib.rs`** so it builds

```rust
#[allow(warnings)]
mod bindings; // generated by cargo component
use bindings::exports::greentic::extension_design::tools::Guest as ToolsGuest;
use bindings::greentic::extension_base::types::ExtensionError;

struct Component;
impl ToolsGuest for Component {
    fn invoke_tool(name: String, args_json: String) -> Result<String, ExtensionError> {
        match name.as_str() {
            "complete" => crate::complete::run(&args_json)
                .map_err(ExtensionError::InvalidInput),
            other => Err(ExtensionError::InvalidInput(format!("unknown tool `{other}`"))),
        }
    }
}
bindings::export!(Component with_types_in bindings);

mod complete { pub fn run(_args: &str) -> Result<String, String> { Err("unimplemented".into()) } }
```

- [ ] **Step 4: Build to verify the component compiles**

Run:
```bash
cd greentic-llm-extensions/reference-extensions/llm-bridge
cargo component build --release --target wasm32-wasip2
wasm-tools validate target/wasm32-wasip2/release/llm_bridge.wasm
```
Expected: builds + validates.

- [ ] **Step 5: Commit**

```bash
git add reference-extensions/llm-bridge
git commit -m "feat(llm-bridge): scaffold extension with complete tool stub"
```

### Task 3.2: Wire types mirrored from the runner

**Files:**
- Create: `.../llm-bridge/src/wire.rs`

- [ ] **Step 1: Write a serde roundtrip test for the BridgeRequest shape**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_bridge_request_from_runner() {
        let raw = r#"{"system_prompt":"hi","history":[{"role":"user","content":"yo"}],
          "tools":[],"credential":{"provider":"openai","model":"gpt-4o-mini","api_key":"sk"}}"#;
        let req: BridgeRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.credential.provider, "openai");
        assert!(matches!(req.history[0], ChatMessage::User { .. }));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llm-bridge wire -- --nocapture` (native build via the `rlib` crate-type — add `crate-type = ["cdylib","rlib"]`).
Expected: FAIL — types missing.

- [ ] **Step 3: Implement `wire.rs` mirroring the runner's exact serde shapes**

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
pub struct BridgeRequest {
    pub system_prompt: String,
    pub history: Vec<ChatMessage>,
    pub tools: Vec<LlmToolSchema>,
    pub credential: BridgeCredential,
}

#[derive(Deserialize)]
pub struct BridgeCredential {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Deserialize)]
pub struct LlmToolSchema {
    pub extension_id: String,
    pub tool_name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System { content: String },
    User { content: String },
    Assistant { content: String, tool_calls: Vec<ToolCallRecord> },
    Tool { call_id: String, content: Value },
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ToolCallRecord {
    pub call_id: String,
    pub extension_id: String,
    pub tool_name: String,
    pub args: Value,
}

#[derive(Serialize)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens_in: u32,
    pub tokens_out: u32,
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p llm-bridge wire -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add reference-extensions/llm-bridge/src/wire.rs reference-extensions/llm-bridge/Cargo.toml
git commit -m "feat(llm-bridge): wire types mirroring runner BridgeRequest/LlmResponse"
```

### Task 3.3: Provider registry + dispatch

**Files:**
- Create: `.../llm-bridge/src/registry.rs`
- Modify: `.../llm-bridge/src/complete.rs`

- [ ] **Step 1: Write the failing test for slug→family/base_url**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_slugs_to_families() {
        assert_eq!(family_of("groq"), Some(Family::OpenAi));
        assert_eq!(family_of("anthropic"), Some(Family::Anthropic));
        assert_eq!(family_of("gemini"), Some(Family::Gemini));
        assert_eq!(family_of("cohere"), Some(Family::Cohere));
        assert_eq!(default_base_url("groq"), Some("https://api.groq.com/openai"));
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llm-bridge registry -- --nocapture`; Expected: FAIL.

- [ ] **Step 3: Implement the registry** (use the table from the plan header verbatim)

```rust
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Family { OpenAi, Anthropic, Gemini, Cohere }

pub fn family_of(slug: &str) -> Option<Family> {
    Some(match slug {
        "openai" | "deepseek" | "groq" | "perplexity" | "xai" | "ollama" | "openai-compatible" => Family::OpenAi,
        "anthropic" => Family::Anthropic,
        "gemini" => Family::Gemini,
        "cohere" => Family::Cohere,
        _ => return None,
    })
}

pub fn default_base_url(slug: &str) -> Option<&'static str> {
    Some(match slug {
        "openai" => "https://api.openai.com",
        "deepseek" => "https://api.deepseek.com",
        "groq" => "https://api.groq.com/openai",
        "perplexity" => "https://api.perplexity.ai",
        "xai" => "https://api.x.ai",
        "ollama" => "http://localhost:11434",
        "anthropic" => "https://api.anthropic.com",
        "gemini" => "https://generativelanguage.googleapis.com",
        "cohere" => "https://api.cohere.com",
        _ => return None, // openai-compatible MUST set credential.base_url
    })
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p llm-bridge registry -- --nocapture`; Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add reference-extensions/llm-bridge/src/registry.rs
git commit -m "feat(llm-bridge): 9-provider registry (slug→family/base_url)"
```

### Task 3.4: OpenAI-family mapper (covers 7 providers)

**Files:**
- Create: `.../llm-bridge/src/providers/openai.rs`

Port the runner's `llm_openai.rs` request/response shape (see spec "Canonical facts"). The mapper takes `(&BridgeRequest, base_url)` → builds the `/v1/chat/completions` body, calls `host_http::fetch`, parses into `LlmResponse`.

- [ ] **Step 1: Write the failing test** (pure request-building + response-parsing; no network — split the I/O out)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::*;
    #[test]
    fn builds_openai_body_with_system_and_user() {
        let req = sample_request("openai", "gpt-4o-mini");
        let body = build_body(&req);
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
    }
    #[test]
    fn parses_openai_response_content_and_usage() {
        let raw = serde_json::json!({
            "choices":[{"message":{"content":"hello","tool_calls":null}}],
            "usage":{"prompt_tokens":10,"completion_tokens":3}
        });
        let resp = parse_response(raw).unwrap();
        assert_eq!(resp.content.as_deref(), Some("hello"));
        assert_eq!(resp.tokens_in, 10);
        assert_eq!(resp.tokens_out, 3);
    }
}
```
(Provide a `sample_request` test helper in `wire.rs` under `#[cfg(test)]`.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p llm-bridge providers::openai -- --nocapture`; Expected: FAIL.

- [ ] **Step 3: Implement `build_body` + `parse_response` + `call`** (mirror `llm_openai.rs`: system prompt first, map history, `name = "{extension_id}.{tool_name}"`, `tool_choice:"auto"`, parse `choices[0].message`, `usage.prompt_tokens/completion_tokens`, tool_calls via `function.name`/`function.arguments` → split on first `.`). Use `host_http::fetch` for `call`. Keep `build_body`/`parse_response` pure (serde_json::Value) so they unit-test without WASM.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p llm-bridge providers::openai -- --nocapture`; Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add reference-extensions/llm-bridge/src/providers/openai.rs
git commit -m "feat(llm-bridge): OpenAI-family mapper (chat/completions)"
```

### Task 3.5: Anthropic mapper

**Files:** Create `.../llm-bridge/src/providers/anthropic.rs`.

Anthropic differences (bake into tests + impl): endpoint `POST {base}/v1/messages`; headers `x-api-key: {api_key}` + `anthropic-version: 2023-06-01`; body `{ model, max_tokens, system, messages:[{role:"user"|"assistant", content}] }` (system is a top-level field, not a message); response `content[0].text`, usage `usage.input_tokens`/`usage.output_tokens`. Tool use maps `tools:[{name,description,input_schema}]` and `content[].type=="tool_use"` → `ToolCallRecord`.

- [ ] **Step 1:** Write `build_body`/`parse_response` failing tests with an Anthropic sample (system as top-level, `content[].text`, `usage.input_tokens`).
- [ ] **Step 2:** Run → FAIL. `cargo test -p llm-bridge providers::anthropic`
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(llm-bridge): Anthropic mapper (/v1/messages)`.

### Task 3.6: Gemini mapper

**Files:** Create `.../llm-bridge/src/providers/gemini.rs`.

Gemini differences: endpoint `POST {base}/v1beta/models/{model}:generateContent?key={api_key}`; body `{ system_instruction:{parts:[{text}]}, contents:[{role:"user"|"model", parts:[{text}]}] }`; response `candidates[0].content.parts[0].text`, usage `usageMetadata.promptTokenCount`/`candidatesTokenCount`. (Tool calling optional; if absent, return text only.)

- [ ] **Step 1–5:** same TDD rhythm; tests assert the `:generateContent` URL build, `contents[].role` mapping, and `candidates[0]...text` parse. Commit `feat(llm-bridge): Gemini mapper`.

### Task 3.7: Cohere mapper

**Files:** Create `.../llm-bridge/src/providers/cohere.rs`.

Cohere differences: endpoint `POST {base}/v2/chat`; Bearer auth; body `{ model, messages:[{role,content}] }`; response `message.content[0].text`, usage `usage.tokens.input_tokens`/`output_tokens`.

- [ ] **Step 1–5:** same rhythm. Commit `feat(llm-bridge): Cohere mapper`.

### Task 3.8: Dispatch in `complete::run`

**Files:** Modify `.../llm-bridge/src/complete.rs`.

- [ ] **Step 1: Write the failing test** — `run` routes by `credential.provider` and returns serialized `LlmResponse`. Test the routing decision via a pure `route_family(provider)->Family` + assert each mapper's `build_body` is selected (or test end-to-end with a mocked `host_http` if a seam exists; otherwise test `route_family`).

- [ ] **Step 2:** Run → FAIL.

- [ ] **Step 3: Implement**

```rust
use crate::registry::{family_of, default_base_url, Family};
use crate::wire::{BridgeRequest, LlmResponse};

pub fn run(args_json: &str) -> Result<String, String> {
    let req: BridgeRequest = serde_json::from_str(args_json).map_err(|e| e.to_string())?;
    let base = req.credential.base_url.clone()
        .or_else(|| default_base_url(&req.credential.provider).map(str::to_string))
        .ok_or_else(|| format!("no base_url for `{}`", req.credential.provider))?;
    let fam = family_of(&req.credential.provider)
        .ok_or_else(|| format!("unknown provider `{}`", req.credential.provider))?;
    let resp: LlmResponse = match fam {
        Family::OpenAi => crate::providers::openai::call(&req, &base)?,
        Family::Anthropic => crate::providers::anthropic::call(&req, &base)?,
        Family::Gemini => crate::providers::gemini::call(&req, &base)?,
        Family::Cohere => crate::providers::cohere::call(&req, &base)?,
    };
    serde_json::to_string(&resp).map_err(|e| e.to_string())
}
```

- [ ] **Step 4:** Run all bridge tests + `cargo component build --release` + `wasm-tools validate`. Expected: PASS + valid component.

- [ ] **Step 5:** Commit `feat(llm-bridge): route complete() by provider family`.

### Task 3.9: Build + publish pipeline

**Files:** Adapt `build.sh` + `.github/workflows/release.yml` from `llm-openai`.

- [ ] **Step 1:** Copy `build.sh`; adjust crate name to `llm_bridge`, output `.gtxpack` id `greentic.llm-bridge`.
- [ ] **Step 2:** Run `bash build.sh` locally → produces `dist/greentic.llm-bridge-<version>.gtxpack`; `gtdx validate` passes.
- [ ] **Step 3:** Add a `release.yml` job using `greenticai/greentic-designer-extension-action@v2` with `gtdx-version: "=1.2.0-research"` (match sibling extensions) + `GREENTIC_STORE_TOKEN`.
- [ ] **Step 4:** Verify CI dry-run / `gtdx validate dist/*.gtxpack`.
- [ ] **Step 5:** Commit `ci(llm-bridge): build + Store publish`.

---

## Phase 4 — Designer: carry the credential ref into the worker config

Work in `greentic-designer` (branch off `research`).

### Task 4.1: `dw_form_to_agent_config` populates `credential_ref` + provider slug

**Files:**
- Modify: `greentic-designer/src/orchestrate/dw_form_to_agent_config.rs:41-61`

- [ ] **Step 1: Write the failing test** — given a form with `provider_id="provider.llm.anthropic.chat"`, `credentialRef="cred-uuid"`, `params.model="claude-3-5-sonnet-latest"`, the produced `AgentConfig.llm` is `{ provider:"anthropic", model:"claude-3-5-sonnet-latest", credential_ref:Some("cred-uuid") }`.

```rust
    #[test]
    fn maps_provider_id_to_slug_and_carries_credential_ref() {
        let form = sample_form_with("provider.llm.anthropic.chat", "cred-uuid", "claude-3-5-sonnet-latest");
        let cfg = dw_form_to_agent_config(&form);
        assert_eq!(cfg.llm.provider, "anthropic");
        assert_eq!(cfg.llm.model, "claude-3-5-sonnet-latest");
        assert_eq!(cfg.llm.credential_ref.as_deref(), Some("cred-uuid"));
    }
```

- [ ] **Step 2:** Run → FAIL. `cargo test -p <designer-crate> dw_form_to_agent_config`
- [ ] **Step 3: Implement** a `provider_id → slug` helper (`provider.llm.{slug}.{variant}` → `{slug}`) and set `credential_ref` from the form's selected credential. Bump the `greentic-aw-runtime`/config dependency so `LlmProviderRef` has the new field (Task 2.1 must be released/pinned first, or use a path/patch during dev).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5:** Commit `feat(designer): carry credential_ref + provider slug into AgentConfig`.

---

## Phase 5 — Catalog: 9 provider entries

### Task 5.1: Add 9 entries to the source catalog + refresh

**Files:**
- Modify: `greentic-dw/examples/providers/catalog.json`
- Run: `greentic-designer/scripts/refresh-dw-catalog.sh`
- Result: `greentic-designer/assets/dw-providers-catalog.json`

- [ ] **Step 1: Write/extend a catalog test** asserting all 9 `cap://llm/chat` entries parse and expose a model field + credential-ref question. (If `greentic-dw` has `starter_catalogs_tests.rs`, extend it; else add a JSON-validity test.)
- [ ] **Step 2:** Run → FAIL (only 2 entries today).
- [ ] **Step 3:** Add entries for openai, anthropic, deepseek, gemini, cohere, ollama, groq, perplexity, xai. Each: `provider_id="provider.llm.{slug}.chat"`, `family:"llm"`, `category:"chat"`, `capability_profile.capability_contract_ids:["cap://llm/chat"]`, `brand:"{slug}"`, `display_name`, `summary`, a model question block (default from the registry table) and a credential-ref question block. Use the existing OpenAI entry shape as the template.
- [ ] **Step 4:** Run the catalog test → PASS; run `refresh-dw-catalog.sh`; confirm `assets/dw-providers-catalog.json` now has 9 `cap://llm/chat` entries.
- [ ] **Step 5:** Commit in both repos: `feat(catalog): 9 LLM provider entries` (greentic-dw) and `chore(designer): refresh dw catalog with 9 LLM providers`.

---

## Phase 6 — Deploy + live smoke

### Task 6.1: Operator env + broker-backed admin facade

**Files:**
- Modify: `greentic-start` / ECS task definition env (per `reference_*` deploy docs).

- [ ] **Step 1:** Set on the runner task: `GREENTIC_AW_LLM_EXTENSION=greentic.llm-bridge`, `SECRETS_BACKEND=broker`, `SECRETS_BROKER_ENDPOINT=<broker url>`, `SECRETS_BROKER_TOKEN=<token>`, and ensure `GREENTIC_AW_REDIS_URL` is set (existing requirement).
- [ ] **Step 2:** Confirm the admin facade in the same environment is **broker-backed** (so admin writes land where the runner reads). Verify by writing a test LLM provider for a tenant in admin, then `oras`/curl GET the broker key.
- [ ] **Step 3:** Install the `greentic.llm-bridge` extension into the operator (per the install path) and confirm `ext_runtime.invoke_tool("greentic.llm-bridge","complete",…)` resolves (check runner logs for "AW LLM via bridge (per-tenant creds)").

### Task 6.2: Live smoke (Anthropic)

- [ ] **Step 1:** In the designer, build a digital worker, pick **Anthropic** + a model + the tenant's Anthropic credential; publish/deploy.
- [ ] **Step 2:** Send a message to the worker via the operator's chat/ingress.
- [ ] **Step 3:** Assert the reply comes from Anthropic (check runner logs: provider=anthropic, a real completion, token usage > 0; confirm no OpenAI call). Repeat for OpenAI as a regression.
- [ ] **Step 4:** Record the smoke result in the spec's status section + update memory `project_multi_provider_llm_bridge`.

---

## Self-review notes (author)

- **Spec coverage:** components 1 (bridge, Phase 3) / 2 (runner creds+secrets, Phases 1–2) / 3 (catalog, Phase 5) / 4 (creds foundation reuse — broker read, Phase 1 + deploy Phase 6) / 5 (deploy, Phase 6) all have tasks. The credential-ref decision (designer) is Phase 4.
- **Open contract #1 (bridge tool interface):** resolved — `extension-design/tools@0.2.0 invoke-tool`, Phase 3.1.
- **Open contract #2 (identifier chain):** resolved — credential_ref UUID in config, Phases 2.1/4.1.
- **Open contract #3 (secret URI shape):** pinned by Phase 0 round-trip + Task 2.2 builds `secrets://default/{tenant}/_/llm/{ref}`.
- **Cross-repo dependency:** Task 2.1 (LlmProviderRef field) must land/pin before Task 4.1 (designer consumes it). During dev, use a path/patch dependency; for release, pin the published `greentic-aw-runtime` version.
- **Streaming:** the bridge implements only `complete`; `ExtensionLlmBackend` does not override `complete_streaming`, so the default (single delta) applies — acceptable for v1.
