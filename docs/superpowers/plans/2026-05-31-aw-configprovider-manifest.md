# Agentic-Worker Tools Live — ConfigProvider + Manifest Overlay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a Digital Worker's extension tools live for the agentic-worker loop: prove the existing YAML tool path end-to-end (Phase A), add a manifest-driven tool *overlay* so a designer-composed tool set flows in without hand-listing (Phase B), and deploy it inside the `gtc start` bundle via a git dependency that enables the `agentic-worker` feature.

**Architecture:** `AgentConfig` (system_prompt + llm + limits + tools) is delivered by a `ConfigProvider`. Today `HostConfigProvider` reads the whole thing from operator YAML. The `DigitalWorkerManifest` carries **only** the tool snapshot (no prompt, no llm). So Phase B is a decorator — `ManifestToolOverlayProvider<P>` — that resolves the YAML base from an inner provider and, when a `<agent_id>.json` manifest exists on disk, replaces `base.tools` with `manifest_to_tool_refs(&manifest)`. Fail-soft: any manifest problem logs a warning and returns the YAML base unchanged. The whole stack is wrapped in the existing `CachingConfigProvider`.

**Tech Stack:** Rust (edition 2024), Tokio, `serde_json`, `greentic-dw-manifest` (already a dep), `greentic-ext-runtime` (already a dep), `greentic-extension-sdk-testing` (new dev-dep for Phase A), `tempfile` (new dev-dep for Phase B). Tests behind the existing `test-mock` feature where mocks are used.

**Spec:** `docs/superpowers/specs/2026-05-31-aw-configprovider-manifest-design.md`

**Branch:** all work on `research` (per the standing project instruction). Commit per step; do NOT add AI attribution to commits.

---

## File Structure

| File | Responsibility | Task |
|------|----------------|------|
| `greentic-runner/docs/agentic-worker-tools.md` (new) | Operator doc: enable tools via YAML `agents:` + via manifest | 1, 6 |
| `greentic-runner/crates/greentic-aw-runtime/Cargo.toml` (modify) | Add dev-deps (`tempfile`; Phase A test deps) | 2, 3 |
| `greentic-runner/crates/greentic-aw-runtime/tests/tools_live.rs` (new) | Phase A: real fixture extension → loop lists + dispatches a tool | 2 |
| `greentic-runner/crates/greentic-aw-runtime/src/manifest_provider.rs` (new) | Phase B: `ManifestToolOverlayProvider` + unit tests | 3 |
| `greentic-runner/crates/greentic-aw-runtime/src/lib.rs` (modify) | Re-export the new provider | 4 |
| `greentic-runner/crates/greentic-runner-host/src/runner/agent_node.rs` (modify) | `manifests_discovery_dir()` + wrap provider in `build_agent_node_handler` | 5 |
| `greentic-start/Cargo.toml` (modify) | git-dep host+desktop @ research tag, enable `agentic-worker` | 7 |
| `greentic-start/docs/agentic-worker-bundle.md` (new) | Operator deployment doc (env, redis, manifests dir) | 7 |

---

## Task 1: Phase A — operator doc for the YAML tool path

**Files:**
- Create: `greentic-runner/docs/agentic-worker-tools.md`

- [ ] **Step 1: Write the doc**

Create `greentic-runner/docs/agentic-worker-tools.md` with this content:

````markdown
# Enabling extension tools for a Digital Worker (agentic worker)

An agentic-worker agent (`DwAgent` flow node) calls extension tools through the
`greentic-ext-runtime`. Each tool the agent may use is declared as a
`ToolRef { extension_id, tool_name }` in the agent's `AgentConfig`.

## Method 1 — declare tools in operator YAML (always available)

The runner-host config (`HostConfig`) carries an `agents:` map. Each entry is a
full `AgentConfig`: the system prompt, the LLM provider/model, limits, and the
tool list.

```yaml
agents:
  research-bot:
    agent_id: research-bot
    system_prompt: "You are a research assistant. Use tools when helpful."
    llm:
      provider: openai
      model: gpt-4o-mini
    tools:
      - extension_id: greentic.tavily
        tool_name: web_search
      - extension_id: greentic.sql
        tool_name: sql_ask
    limits:
      max_iter: 8
      timeout: 60
      max_history_turns: 20
      llm_retry_attempts: 3
      llm_retry_backoff: 250
```

At runtime the loop resolves each `ToolRef` against the loaded extension via
`ExtensionRuntime::list_tools` — a tool whose extension is not installed is
logged and silently skipped (the LLM simply never sees it).

Prerequisites:
- The extension is installed in the extension discovery dir
  (`GREENTIC_EXTENSIONS_DIR`, else `~/.greentic/extensions`).
- `GREENTIC_AW_REDIS_URL` is set (the agent loop persists session state in Redis).

## Method 2 — auto-overlay tools from a Digital Worker manifest

See `docs/agentic-worker-tools.md#manifest-overlay` (added in Task 6) once the
manifest overlay ships. In short: drop the DW's `<agent_id>.json` manifest into
the manifests dir and its `agentic_worker`-capable tools replace the YAML
`tools:` list automatically; the YAML still supplies `system_prompt` + `llm`.
````

- [ ] **Step 2: Commit**

```bash
cd greentic-runner
git add docs/agentic-worker-tools.md
git commit -m "docs(aw): operator guide for enabling extension tools via YAML"
```

---

## Task 2: Phase A — integration test proving tools are listed + dispatched

This test loads a **real** signed fixture extension (the `greentic-ext-runtime`
`runtime_load.rs` / `scaffold_e2e.rs` pattern) and asserts the AW tool helpers
resolve and dispatch it. `ExtensionRuntime::for_test()` is empty and cannot be
used for a positive dispatch assertion.

**Files:**
- Modify: `greentic-runner/crates/greentic-aw-runtime/Cargo.toml` (dev-deps)
- Create: `greentic-runner/crates/greentic-aw-runtime/tests/support/mod.rs`
- Create: `greentic-runner/crates/greentic-aw-runtime/tests/tools_live.rs`

- [ ] **Step 1: Read the proven fixture helper to copy it verbatim**

Open and read these two files end-to-end (they are the source of truth for the
signing/gtpack dance — copy, do not reinvent):
- `greentic-designer-extensions/crates/greentic-ext-runtime/tests/support/mod.rs`
  (functions `signed_fixture`, `populate_gtpack_for_local_load`,
  `build_dir_manifest_bytes`, `finalize_signed_with_manifest`)
- `greentic-designer-extensions/crates/greentic-ext-runtime/tests/runtime_load.rs`
  (the minimal load+assert flow)

Note the exact dev-dependencies that crate's `Cargo.toml` declares for those
tests (`greentic-extension-sdk-testing`, `greentic-extension-sdk-contract`,
`ed25519-dalek`, `serde_jcs`, `tempfile`) and their git/version coordinates.

- [ ] **Step 2: Add dev-dependencies to aw-runtime**

Edit `greentic-runner/crates/greentic-aw-runtime/Cargo.toml` `[dev-dependencies]`
to add (mirror the coordinates found in Step 1 — these crates already resolve in
the workspace because `greentic-ext-runtime` pulls them transitively):

```toml
[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync", "test-util"] }
tempfile = "3"
greentic-extension-sdk-testing  = { git = "https://github.com/greentic-biz/greentic-designer-sdk", branch = "research" }
greentic-extension-sdk-contract = { git = "https://github.com/greentic-biz/greentic-designer-sdk", branch = "research" }
ed25519-dalek = { version = "2", features = ["rand_core"] }
serde_jcs = "0.1"
```

Also add a test entry so this test only builds with `test-mock` (it uses the mock
LLM/state doubles). Append to `Cargo.toml`:

```toml
[[test]]
name = "tools_live"
required-features = ["test-mock"]
```

- [ ] **Step 3: Vendor the fixture helper**

Create `greentic-runner/crates/greentic-aw-runtime/tests/support/mod.rs` and copy
the `signed_fixture`, `populate_gtpack_for_local_load`, `build_dir_manifest_bytes`,
and `finalize_signed_with_manifest` functions **verbatim** from the file read in
Step 1 (adjust only the `#![allow(dead_code)]` and imports to compile here).
`signed_fixture(ExtensionKind::Design, id, version)` returns
`(ExtensionFixture, SigningKey)` and the scaffolded fixture exports at least one
agentic-worker tool (per `scaffold_e2e.rs`).

- [ ] **Step 4: Write the failing test**

Create `greentic-runner/crates/greentic-aw-runtime/tests/tools_live.rs`:

```rust
//! Phase A: prove the YAML tool path is live — a loaded extension's tool is
//! listed for the LLM and dispatched through the AW tool helpers.

mod support;

use std::sync::Arc;

use greentic_aw_runtime::config::ToolRef;
use greentic_aw_runtime::state::ToolCallRecord;
use greentic_aw_runtime::tools::{dispatch_tool_call, list_tools_for_llm};
use greentic_ext_runtime::{DiscoveryPaths, ExtensionRuntime, RuntimeConfig};

#[tokio::test(flavor = "multi_thread")]
async fn loaded_extension_tool_is_listed_and_dispatched() {
    // 1. Build a signed design fixture that exports an agentic-worker tool.
    let (fixture, _signing_key) = support::signed_fixture(
        greentic_extension_sdk_contract::ExtensionKind::Design,
        "greentic.test-ext",
        "0.1.0",
    );

    // 2. Construct a runtime pointed at the fixture and load it.
    let config = RuntimeConfig::from_paths(DiscoveryPaths::new(
        fixture.root().to_path_buf(),
    ));
    let mut runtime = ExtensionRuntime::new(config).expect("runtime builds");
    runtime
        .register_loaded_from_dir(fixture.root())
        .expect("fixture loads");

    // 3. The extension exposes at least one tool.
    let tool_defs = runtime
        .list_tools("greentic.test-ext")
        .expect("list_tools succeeds for a loaded extension");
    assert!(!tool_defs.is_empty(), "fixture should export a tool");
    let tool_name = tool_defs[0].name.clone();

    // 4. list_tools_for_llm resolves the ToolRef into an LLM schema.
    let allowed = vec![ToolRef {
        extension_id: "greentic.test-ext".into(),
        tool_name: tool_name.clone(),
    }];
    let schemas = list_tools_for_llm(&runtime, &allowed);
    assert_eq!(schemas.len(), 1, "the allow-listed tool must be visible to the LLM");
    assert_eq!(schemas[0].tool_name, tool_name);

    // 5. dispatch_tool_call invokes the tool through the runtime. The scaffolded
    //    fixture tool returns a stub result/error; we assert dispatch REACHED the
    //    tool (Ok, or an invoke error from the stub), never a "tool not found".
    let call = ToolCallRecord {
        call_id: "call-1".into(),
        extension_id: "greentic.test-ext".into(),
        tool_name,
        args: serde_json::json!({}),
    };
    let result = dispatch_tool_call(Arc::new(runtime), call).await;
    match result {
        Ok(_) => {}
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("invoke") || msg.contains("decode"),
                "dispatch must reach the tool (stub invoke/decode error ok), got: {msg}"
            );
        }
    }
}
```

- [ ] **Step 5: Run the test to verify it builds and passes**

Run: `cargo test -p greentic-aw-runtime --features test-mock --test tools_live -- --nocapture`
Expected: PASS. If the fixture export name or load path differs, fix against the
real `runtime_load.rs` behaviour observed in Step 1 (do not stub).

- [ ] **Step 6: Commit**

```bash
cd greentic-runner
git add crates/greentic-aw-runtime/Cargo.toml crates/greentic-aw-runtime/tests/
git commit -m "test(aw): prove loaded extension tool is listed and dispatched (Phase A)"
```

---

## Task 3: Phase B — `ManifestToolOverlayProvider` (new module, TDD)

**Files:**
- Modify: `greentic-runner/crates/greentic-aw-runtime/Cargo.toml` (`tempfile` already added in Task 2 Step 2; no change if done)
- Create: `greentic-runner/crates/greentic-aw-runtime/src/manifest_provider.rs`

- [ ] **Step 1: Write the failing unit tests first**

Create `greentic-runner/crates/greentic-aw-runtime/src/manifest_provider.rs` with
ONLY the test module (the impl comes in Step 3). Use a pure-JSON manifest fixture
(no extra git deps): the locale enums are `snake_case`, `TeamPolicy::Disabled`
serialises as `"disabled"`, `version` defaults to `"0.3"`, and
`agentic_worker_metadata: {}` deserialises (all fields are `Option`).

```rust
//! A [`ConfigProvider`] decorator that overlays a Digital Worker manifest's
//! tool set onto a base config. The manifest supplies ONLY `tools`; the inner
//! provider remains authoritative for `system_prompt` / `llm` / `limits`.
//!
//! Fail-soft: a missing, malformed, or mismatched manifest logs a warning and
//! returns the base config unchanged, so a broken manifest never takes an agent
//! offline (it degrades to the operator's YAML tool list).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use greentic_dw_manifest::DigitalWorkerManifest;

use crate::config::AgentConfig;
use crate::config_provider::ConfigProvider;
use crate::error::ConfigError;
use crate::manifest_tools::manifest_to_tool_refs;
use crate::tenant::TenantContext;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::{AgentLimits, LlmProviderRef, ToolRef};
    use crate::config_provider::InMemoryConfigProvider;

    fn base_provider(agent_id: &str, tenant: &TenantContext) -> InMemoryConfigProvider {
        let mut p = InMemoryConfigProvider::new();
        p.insert(
            tenant,
            agent_id,
            AgentConfig {
                agent_id: agent_id.into(),
                system_prompt: "yaml-prompt".into(),
                tools: vec![ToolRef {
                    extension_id: "yaml.ext".into(),
                    tool_name: "yaml_tool".into(),
                }],
                llm: LlmProviderRef { provider: "openai".into(), model: "gpt-4o-mini".into() },
                limits: AgentLimits::default(),
            },
        );
        p
    }

    /// Minimal valid v0.3 manifest JSON exporting one agentic-worker tool.
    fn manifest_json(agent_id: &str, ext_id: &str, tool: &str) -> String {
        format!(
            r#"{{
              "id": "{agent_id}",
              "display_name": "Test Worker",
              "tenancy": {{ "tenant": "t", "team_policy": "disabled" }},
              "locale": {{
                "worker_default_locale": "en-US",
                "policy": "worker_default",
                "propagation": "current_task_only",
                "output": "worker_default"
              }},
              "extension_tools": [
                {{
                  "extension_id": "{ext_id}",
                  "extension_version": "1.0.0",
                  "tool_name": "{tool}",
                  "description": "desc",
                  "input_schema_json": "{{\"type\":\"object\"}}",
                  "capabilities": ["agentic_worker"],
                  "agentic_worker_metadata": {{}}
                }}
              ]
            }}"#
        )
    }

    fn write_manifest(dir: &std::path::Path, agent_id: &str, body: &str) {
        std::fs::write(dir.join(format!("{agent_id}.json")), body).unwrap();
    }

    #[tokio::test]
    async fn overlays_manifest_tools_over_base() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantContext::new("t", "e");
        write_manifest(tmp.path(), "bot", &manifest_json("bot", "greentic.tavily", "web_search"));

        let provider = ManifestToolOverlayProvider::new(
            base_provider("bot", &tenant),
            tmp.path().to_path_buf(),
        );
        let cfg = provider.agent_config(&tenant, "bot").await.unwrap();

        // tools come from the manifest…
        assert_eq!(cfg.tools, vec![ToolRef {
            extension_id: "greentic.tavily".into(),
            tool_name: "web_search".into(),
        }]);
        // …prompt + llm stay from the YAML base.
        assert_eq!(cfg.system_prompt, "yaml-prompt");
        assert_eq!(cfg.llm.model, "gpt-4o-mini");
    }

    #[tokio::test]
    async fn returns_base_unchanged_when_manifest_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantContext::new("t", "e");
        let provider = ManifestToolOverlayProvider::new(
            base_provider("bot", &tenant),
            tmp.path().to_path_buf(),
        );
        let cfg = provider.agent_config(&tenant, "bot").await.unwrap();
        assert_eq!(cfg.tools, vec![ToolRef {
            extension_id: "yaml.ext".into(),
            tool_name: "yaml_tool".into(),
        }]);
    }

    #[tokio::test]
    async fn returns_base_unchanged_when_manifest_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantContext::new("t", "e");
        write_manifest(tmp.path(), "bot", "{not valid json");
        let provider = ManifestToolOverlayProvider::new(
            base_provider("bot", &tenant),
            tmp.path().to_path_buf(),
        );
        let cfg = provider.agent_config(&tenant, "bot").await.unwrap();
        assert_eq!(cfg.tools[0].extension_id, "yaml.ext");
    }

    #[tokio::test]
    async fn ignores_manifest_with_mismatched_id() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantContext::new("t", "e");
        // file named bot.json but manifest.id = "other"
        write_manifest(tmp.path(), "bot", &manifest_json("other", "greentic.tavily", "web_search"));
        let provider = ManifestToolOverlayProvider::new(
            base_provider("bot", &tenant),
            tmp.path().to_path_buf(),
        );
        let cfg = provider.agent_config(&tenant, "bot").await.unwrap();
        assert_eq!(cfg.tools[0].extension_id, "yaml.ext");
    }

    #[tokio::test]
    async fn propagates_inner_agent_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let tenant = TenantContext::new("t", "e");
        let provider = ManifestToolOverlayProvider::new(
            InMemoryConfigProvider::new(), // empty base
            tmp.path().to_path_buf(),
        );
        let result = provider.agent_config(&tenant, "missing").await;
        assert!(matches!(result, Err(ConfigError::AgentNotFound(_))));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail (no impl yet)**

Run: `cargo test -p greentic-aw-runtime manifest_provider`
Expected: FAIL to compile — `ManifestToolOverlayProvider` not defined.

- [ ] **Step 3: Write the implementation**

Prepend the impl above the test module in `src/manifest_provider.rs`:

```rust
/// Wraps a base [`ConfigProvider`] and overlays a Digital Worker manifest's
/// agentic-worker tool set onto `AgentConfig.tools`. See module docs for the
/// fail-soft contract.
pub struct ManifestToolOverlayProvider<P: ConfigProvider> {
    inner: P,
    manifests_dir: PathBuf,
}

impl<P: ConfigProvider> ManifestToolOverlayProvider<P> {
    /// `manifests_dir` is scanned for `<agent_id>.json` per request.
    pub fn new(inner: P, manifests_dir: PathBuf) -> Self {
        Self { inner, manifests_dir }
    }

    /// Load + validate `<agent_id>.json`. Returns `None` (fail-soft) for absent,
    /// unreadable, malformed, invalid, or id-mismatched manifests, logging a
    /// warning for every problem except a plain absent file (the common case).
    fn load_manifest(&self, agent_id: &str) -> Option<DigitalWorkerManifest> {
        let path = self.manifests_dir.join(format!("{agent_id}.json"));
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(agent_id, error = %e, "manifest read failed; using YAML base");
                return None;
            }
        };
        let manifest: DigitalWorkerManifest = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(agent_id, error = %e, "manifest decode failed; using YAML base");
                return None;
            }
        };
        if let Err(e) = manifest.validate() {
            tracing::warn!(agent_id, error = %e, "manifest invalid; using YAML base");
            return None;
        }
        if manifest.id != agent_id {
            tracing::warn!(
                agent_id, manifest_id = %manifest.id,
                "manifest id does not match filename; ignoring"
            );
            return None;
        }
        Some(manifest)
    }
}

impl<P: ConfigProvider> ConfigProvider for ManifestToolOverlayProvider<P> {
    fn agent_config<'a>(
        &'a self,
        tenant: &'a TenantContext,
        agent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<AgentConfig, ConfigError>> + Send + 'a>> {
        Box::pin(async move {
            let mut base = self.inner.agent_config(tenant, agent_id).await?;
            if let Some(manifest) = self.load_manifest(agent_id) {
                base.tools = manifest_to_tool_refs(&manifest);
            }
            Ok(base)
        })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p greentic-aw-runtime manifest_provider`
Expected: PASS (all 5 tests).

- [ ] **Step 5: Lint + commit**

```bash
cd greentic-runner
cargo clippy -p greentic-aw-runtime --all-targets -- -D warnings
git add crates/greentic-aw-runtime/src/manifest_provider.rs crates/greentic-aw-runtime/Cargo.toml
git commit -m "feat(aw): ManifestToolOverlayProvider overlays manifest tools onto YAML base (Phase B)"
```

---

## Task 4: Phase B — register + re-export the module

**Files:**
- Modify: `greentic-runner/crates/greentic-aw-runtime/src/lib.rs`

- [ ] **Step 1: Declare the module**

In `src/lib.rs`, add to the module list (after `pub mod manifest_tools;`):

```rust
pub mod manifest_provider;
```

- [ ] **Step 2: Re-export the type**

In `src/lib.rs`, extend the `config_provider` re-export line. Change:

```rust
pub use config_provider::{CachingConfigProvider, ConfigProvider, InMemoryConfigProvider};
```
to:
```rust
pub use config_provider::{CachingConfigProvider, ConfigProvider, InMemoryConfigProvider};
pub use manifest_provider::ManifestToolOverlayProvider;
```

- [ ] **Step 3: Build + commit**

Run: `cargo build -p greentic-aw-runtime`
Expected: success.

```bash
cd greentic-runner
git add crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw): export ManifestToolOverlayProvider"
```

---

## Task 5: Phase B — wire the overlay into runner-host

Wrap the existing `HostConfigProvider` in `ManifestToolOverlayProvider`, then in
`CachingConfigProvider`, inside `build_agent_node_handler`. Add a
`manifests_discovery_dir()` resolver mirroring `extension_discovery_dir()`.

**Files:**
- Modify: `greentic-runner/crates/greentic-runner-host/src/runner/agent_node.rs`

- [ ] **Step 1: Add the manifests-dir resolver + its test (write failing test first)**

In `agent_node.rs`, inside `mod aw`, add the resolver next to
`extension_discovery_dir` (around line 144):

```rust
/// Resolve the directory scanned for `<agent_id>.json` Digital Worker manifests.
///
/// Honours `GREENTIC_AGENT_MANIFESTS_DIR`; otherwise `~/.greentic/agents`, and
/// finally a temp-dir path when no home is resolvable (keeps the fn total). A
/// missing dir is harmless — the overlay provider simply finds no manifest and
/// returns the YAML base unchanged.
fn manifests_discovery_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GREENTIC_AGENT_MANIFESTS_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".greentic").join("agents");
    }
    std::env::temp_dir().join("greentic").join("agents")
}
```

Add to the `#[cfg(test)] mod tests` in the same file a test that the overlay is
applied (write before wiring exists so it fails):

```rust
#[tokio::test]
async fn overlay_provider_replaces_tools_from_manifest() {
    use greentic_aw_runtime::ManifestToolOverlayProvider;
    use greentic_aw_runtime::config::ToolRef;
    use greentic_aw_runtime::config_provider::ConfigProvider;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("greeter.json"),
        r#"{"id":"greeter","display_name":"G",
            "tenancy":{"tenant":"t","team_policy":"disabled"},
            "locale":{"worker_default_locale":"en-US","policy":"worker_default",
                      "propagation":"current_task_only","output":"worker_default"},
            "extension_tools":[{"extension_id":"greentic.tavily","extension_version":"1.0.0",
              "tool_name":"web_search","description":"d","input_schema_json":"{\"type\":\"object\"}",
              "capabilities":["agentic_worker"],"agentic_worker_metadata":{}}]}"#,
    )
    .unwrap();

    let mut agents = HashMap::new();
    agents.insert("greeter".to_string(), sample_agent_config("greeter")); // tools: []
    let provider =
        ManifestToolOverlayProvider::new(HostConfigProvider::new(agents), tmp.path().to_path_buf());

    let tenant = TenantContext::new("acme", "prod");
    let cfg = provider.agent_config(&tenant, "greeter").await.unwrap();
    assert_eq!(
        cfg.tools,
        vec![ToolRef { extension_id: "greentic.tavily".into(), tool_name: "web_search".into() }]
    );
}
```

Add `tempfile` to runner-host `[dev-dependencies]` if not already present:
```toml
tempfile = "3"
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p greentic-runner-host --features agentic-worker overlay_provider_replaces_tools_from_manifest`
Expected: FAIL — either won't compile (import) until `ManifestToolOverlayProvider`
is re-exported (Task 4 done) or assertion mismatch. If Task 4 is complete it
should actually PASS already (the provider works standalone); that's fine — this
test locks the behaviour. Proceed to wire it into the handler.

- [ ] **Step 3: Wire the overlay into `build_agent_node_handler`**

In `build_agent_node_handler`, change the provider construction. Find (around
line 300):

```rust
        let config_provider = Arc::new(CachingConfigProvider::new(HostConfigProvider::new(
            config.agents.clone(),
        )));
```
Replace with:
```rust
        let overlay = ManifestToolOverlayProvider::new(
            HostConfigProvider::new(config.agents.clone()),
            manifests_discovery_dir(),
        );
        let config_provider = Arc::new(CachingConfigProvider::new(overlay));
```

Add the import to the `use greentic_aw_runtime::...` block at the top of
`build_agent_node_handler` (near the `CachingConfigProvider` import):

```rust
        use greentic_aw_runtime::ManifestToolOverlayProvider;
```

- [ ] **Step 4: Build + run the test + existing tests**

Run: `cargo test -p greentic-runner-host --features agentic-worker`
Expected: PASS (the new test + all existing `agent_node` tests).

- [ ] **Step 5: Lint + commit**

```bash
cd greentic-runner
cargo clippy -p greentic-runner-host --all-targets --features agentic-worker -- -D warnings
git add crates/greentic-runner-host/src/runner/agent_node.rs crates/greentic-runner-host/Cargo.toml
git commit -m "feat(runner-host): overlay DW manifest tools onto agent config (Phase B wiring)"
```

---

## Task 6: Phase B — operator doc for the manifest overlay

**Files:**
- Modify: `greentic-runner/docs/agentic-worker-tools.md`

- [ ] **Step 1: Append the manifest-overlay section**

Append to `greentic-runner/docs/agentic-worker-tools.md`:

````markdown

## Manifest overlay

Instead of hand-listing `tools:` in YAML, drop the Digital Worker's manifest
JSON into the manifests dir and the runner overlays its `agentic_worker`-capable
tools onto the agent's `tools` list automatically.

- **Where:** `GREENTIC_AGENT_MANIFESTS_DIR` (else `~/.greentic/agents/`), file
  named `<agent_id>.json`.
- **What it is:** the `DigitalWorkerManifest` JSON the DW compose wizard emits
  (`greentic-dw` wizard stdout — capture it to a file).
- **What it overrides:** only `AgentConfig.tools`. The operator YAML `agents:`
  entry still supplies `system_prompt`, `llm`, and `limits` — those are NOT in
  the manifest.
- **Fail-soft:** a missing, malformed, invalid, or id-mismatched manifest is
  logged and ignored; the agent falls back to the YAML `tools:` list. A broken
  manifest never takes the agent offline.

Example: with `~/.greentic/agents/research-bot.json` present, the YAML
`agents.research-bot` needs only `system_prompt` + `llm` (+ optional `limits`);
its `tools:` may be empty and will be replaced by the manifest's tool set at
load time.

> Note: v1 expects a loose `<agent_id>.json`. Auto-extracting the manifest from
> the composed `.gtpack` is a planned follow-up.
````

- [ ] **Step 2: Commit**

```bash
cd greentic-runner
git add docs/agentic-worker-tools.md
git commit -m "docs(aw): document the manifest tool-overlay path"
```

---

## Task 7: Deployment — git-dep greentic-start on research host with `agentic-worker`

The published `1.2.0-research` crates strip `agentic-worker` (+ aw-runtime/
ext-runtime). To run the agent in the bundle, point greentic-start at the
greentic-runner `research` source via git with the feature enabled.

**Files:**
- Modify: `greentic-start/Cargo.toml`
- Create: `greentic-start/docs/agentic-worker-bundle.md`

- [ ] **Step 1: Tag the runner research HEAD for a reproducible pin**

After Tasks 1–6 are merged on `research`:

```bash
cd greentic-runner
git tag aw-overlay-v1
git push origin aw-overlay-v1
```

- [ ] **Step 2: Switch the host + desktop deps to git + enable the feature**

Edit `greentic-start/Cargo.toml`. Replace the two version deps (lines ~84–90):

```toml
[dependencies.greentic-runner-desktop]
version = "=1.2.0-research"
default-features = false

[dependencies.greentic-runner-host]
version = "=1.2.0-research"
default-features = false
```
with:
```toml
[dependencies.greentic-runner-desktop]
git = "https://github.com/greenticai/greentic-runner.git"
tag = "aw-overlay-v1"
default-features = false

[dependencies.greentic-runner-host]
git = "https://github.com/greenticai/greentic-runner.git"
tag = "aw-overlay-v1"
default-features = false
features = ["agentic-worker"]
```

- [ ] **Step 3: Build greentic-start and confirm the feature resolves**

Run: `cargo build -p greentic-start`
Expected: success — the `agentic-worker` feature pulls `greentic-aw-runtime`
(path dep inside the git checkout) + `greentic-ext-runtime` (git tag). If
`build_agent_node_handler` lives behind a path the desktop crate also gates,
enable `features = ["agentic-worker"]` on `greentic-runner-desktop` too and
rebuild. If the resolver reports a version conflict on `greentic-ext-runtime` or
`greentic-dw-manifest`, align greentic-start's pins to the same git tags/branches
the runner uses (`v1.2.11-research` / `research`).

- [ ] **Step 4: Smoke-test the agent path in a bundle**

With a bundle that declares an agent in YAML and has an installed extension:

```bash
export GREENTIC_AW_REDIS_URL=redis://127.0.0.1:6379
export GREENTIC_EXTENSIONS_DIR=$HOME/.greentic/extensions
export GREENTIC_AGENT_MANIFESTS_DIR=$HOME/.greentic/agents   # optional (overlay)
cargo run -p greentic-start -- start ./my-bundle --cloudflared off
```
Expected log lines: `AW runtime constructed` (agent count > 0). Drive a `DwAgent`
flow node and confirm a tool is invoked (trail contains a `tool_call` step). If
`GREENTIC_AW_REDIS_URL` is unset you will see `DwAgent nodes disabled` — that is
the documented graceful-degradation path, not a failure.

- [ ] **Step 5: Write the deployment doc**

Create `greentic-start/docs/agentic-worker-bundle.md`:

````markdown
# Running the agentic worker in a `gtc start` bundle

The agentic worker (`DwAgent` flow nodes) requires the `agentic-worker` feature,
which is **only** present in the git-sourced `greentic-runner-host`
(the published crates strip it). greentic-start pins the runner via git tag
`aw-overlay-v1` with `features = ["agentic-worker"]`.

## Runtime prerequisites

| Env var | Purpose | Default |
|---------|---------|---------|
| `GREENTIC_AW_REDIS_URL` | AW session-state store (REQUIRED; unset → agent disabled) | — |
| `GREENTIC_EXTENSIONS_DIR` | extension discovery dir | `~/.greentic/extensions` |
| `GREENTIC_AGENT_MANIFESTS_DIR` | DW manifest overlay dir | `~/.greentic/agents` |
| `OPENAI_API_KEY` / LLM bridge vars | LLM backend | — |

## Per-agent config

- Operator YAML `agents.<id>`: `system_prompt`, `llm`, optional `limits`, optional
  `tools`.
- Optional `<id>.json` manifest in the manifests dir overlays the tool set (see
  greentic-runner `docs/agentic-worker-tools.md`).
````

- [ ] **Step 6: Commit**

```bash
cd greentic-start
git add Cargo.toml Cargo.lock docs/agentic-worker-bundle.md
git commit -m "feat(start): git-dep runner research with agentic-worker; bundle agent deployment doc"
```

---

## Self-Review

**Spec coverage:**
- Phase A (prove existing path + doc) → Tasks 1, 2. ✅
- Phase B (`ManifestToolOverlayProvider`, tools-overlay, JSON, discovery dir, fail-soft, wiring, caching) → Tasks 3, 4, 5. ✅
- Phase B operator doc → Task 6. ✅
- Deployment Option-2 git-dep + `agentic-worker` → Task 7. ✅
- Spec open item 1 (deep_agent mapping) → resolved in spec; plan overlays tools only. ✅
- Spec open item 4 (git-dep rev pin) → Task 7 Step 1 (tag). ✅
- Spec open item 5 (feature enablement builds) → Task 7 Step 3. ✅
- Spec open item 2 (.gtpack extraction) → explicitly deferred; documented in Task 6. ✅

**Type consistency:** `ManifestToolOverlayProvider::new(inner, manifests_dir: PathBuf)` used identically in Tasks 3, 5. `ToolRef { extension_id, tool_name }`, `AgentConfig { agent_id, system_prompt, tools, llm, limits }`, `ConfigError::AgentNotFound`, `manifest_to_tool_refs(&DigitalWorkerManifest) -> Vec<ToolRef>` — all match the real signatures read from source. JSON fixtures use the confirmed serde strings (`"disabled"`, `worker_default`, `current_task_only`).

**Placeholder scan:** no TBD/TODO; every code step shows full code; the one "copy verbatim" step (Task 2 Step 3) cites the exact source file + functions to copy a known-good, tested helper rather than hand-waving.

**Risk note (surfaced, not hidden):** Task 2's fixture reuse and Task 7's feature-resolution are the two integration risks; both have explicit verify steps and fallbacks rather than silent assumptions.
