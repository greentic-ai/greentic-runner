# LLM Provider as Extension — Slice 3 (Production Runner Wiring) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use `- [ ]` checkboxes.

**Goal:** The production runner runs deployed DwAgent workers on the **LLM bridge extension** (`ExtensionLlmBackend`) when configured, instead of the hardcoded `OpenAiLlmBackend` — with the env-keyed OpenAI path kept as the fallback.

**Architecture:** In `greentic-runner-host`'s `build_agent_node_handler`, select the agent LLM backend: if `GREENTIC_AW_LLM_EXTENSION` names a loaded bridge extension AND an LLM key is present, build `ExtensionLlmBackend` (Slice 1) with a `BridgeCredential` from `GREENTIC_LLM_*` env; else keep `OpenAiLlmBackend`. Same workspace as `greentic-aw-runtime` → direct import, no dep bump. The designer playground is unchanged (keeps its in-process adapter).

**Tech Stack:** Rust (greentic-runner-host + greentic-aw-runtime, same workspace).

**Spec:** `docs/superpowers/specs/2026-05-29-llm-provider-as-extension-design.md` (Slice 3, scoped to the production runner per the 2026-05-29 decision).

**Working dir:** `/Users/bimapangestu/Desktop/Works/personal/greentic/greentic-runner/.worktrees/feat-llm-ext-slice3` (`<WT>`, branch `feat/llm-ext-slice3`). cargo via `cargo --manifest-path <WT>/Cargo.toml -p greentic-runner-host …`; git `git -C <WT> …`. No `cd` into the worktree top.

## Reference (already present, same workspace)
- `greentic_aw_runtime::{ExtensionLlmBackend, BridgeCredential, RetryingLlmBackend, OpenAiLlmBackend, LlmBackend, AgentRuntime}` — Slice 1 merged on research.
- `ExtensionLlmBackend::new(ext_runtime: Arc<ExtensionRuntime>, extension_id: impl Into<String>, credential: BridgeCredential)`.
- `BridgeCredential { provider: String, model: String, api_key: String, base_url: Option<String> }`.
- Target fn: `crates/greentic-runner-host/src/runner/agent_node.rs::build_agent_node_handler` (~line 177); current LLM build at ~215-220; `build_ext_runtime() -> Option<Arc<ExtensionRuntime>>` (~152).

---

### Task 1: Credential helper + backend selection

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/agent_node.rs`

- [ ] **Step 1: Add the pure helper + a failing unit test**

Add (module-level, near the other helpers in `agent_node.rs`):

```rust
/// Build a vault-style `BridgeCredential` from resolved parts. `None` when no
/// API key is present. Defaults: provider "openai", model "gpt-4o". Pure (no
/// env) so it is unit-testable without global state.
fn bridge_credential(
    provider: Option<String>,
    model: Option<String>,
    api_key: String,
    base_url: Option<String>,
) -> Option<greentic_aw_runtime::BridgeCredential> {
    if api_key.trim().is_empty() {
        return None;
    }
    Some(greentic_aw_runtime::BridgeCredential {
        provider: provider.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "openai".into()),
        model: model.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "gpt-4o".into()),
        api_key,
        base_url: base_url.filter(|s| !s.trim().is_empty()),
    })
}
```

Add tests in the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn bridge_credential_defaults_provider_and_model() {
        let c = super::bridge_credential(None, None, "sk-x".into(), None).unwrap();
        assert_eq!(c.provider, "openai");
        assert_eq!(c.model, "gpt-4o");
        assert_eq!(c.api_key, "sk-x");
        assert!(c.base_url.is_none());
    }

    #[test]
    fn bridge_credential_honors_explicit_parts() {
        let c = super::bridge_credential(
            Some("anthropic".into()),
            Some("claude-x".into()),
            "sk-ant".into(),
            Some("https://proxy".into()),
        )
        .unwrap();
        assert_eq!(c.provider, "anthropic");
        assert_eq!(c.model, "claude-x");
        assert_eq!(c.base_url.as_deref(), Some("https://proxy"));
    }

    #[test]
    fn bridge_credential_none_without_key() {
        assert!(super::bridge_credential(Some("openai".into()), None, "  ".into(), None).is_none());
    }
```

- [ ] **Step 2: Run the test → red→green**

Run: `cargo test --manifest-path <WT>/Cargo.toml -p greentic-runner-host bridge_credential`
Expected: FAIL to compile before the helper, PASS after (3 tests). Confirm `greentic_aw_runtime::BridgeCredential` is importable (Slice 1 is on research; this workspace tracks it).

- [ ] **Step 3: Wire backend selection into `build_agent_node_handler`**

Add `ExtensionLlmBackend` (and `BridgeCredential` if you prefer a `use`) + `LlmBackend` to the existing `use greentic_aw_runtime::{…}` import block (currently `OpenAiLlmBackend, OtelTelemetry, RedisAgentStateStore, RetryingLlmBackend`).

Replace the current LLM construction:
```rust
    let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
    let llm = Arc::new(RetryingLlmBackend::new(
        OpenAiLlmBackend::new(openai_key),
        3,
        Duration::from_millis(250),
    ));
```
with:
```rust
    // Prefer the LLM bridge extension when configured (LLM-as-extension);
    // fall back to the env-keyed in-process OpenAI client otherwise.
    let llm: Arc<dyn LlmBackend> = match std::env::var("GREENTIC_AW_LLM_EXTENSION")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(ext_id) => {
            let api_key = std::env::var("GREENTIC_LLM_API_KEY")
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_default();
            match bridge_credential(
                std::env::var("GREENTIC_LLM_PROVIDER").ok(),
                std::env::var("GREENTIC_LLM_MODEL").ok(),
                api_key,
                std::env::var("GREENTIC_LLM_BASE_URL").ok(),
            ) {
                Some(cred) => {
                    tracing::info!(
                        extension = %ext_id, provider = %cred.provider, model = %cred.model,
                        "AW LLM via bridge extension"
                    );
                    Arc::new(RetryingLlmBackend::new(
                        ExtensionLlmBackend::new(ext_runtime.clone(), ext_id, cred),
                        3,
                        Duration::from_millis(250),
                    ))
                }
                None => {
                    tracing::warn!(
                        "GREENTIC_AW_LLM_EXTENSION set but no LLM API key; \
                         falling back to in-process OpenAI client"
                    );
                    Arc::new(RetryingLlmBackend::new(
                        OpenAiLlmBackend::new(String::new()),
                        3,
                        Duration::from_millis(250),
                    ))
                }
            }
        }
        None => {
            let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            Arc::new(RetryingLlmBackend::new(
                OpenAiLlmBackend::new(openai_key),
                3,
                Duration::from_millis(250),
            ))
        }
    };
```

(`ext_runtime` is the `Arc<ExtensionRuntime>` from `build_ext_runtime()?` above; `.clone()` is cheap. It is still moved into `AgentRuntime::new(…, ext_runtime, …)` afterwards — clone before that move. If the current code moves `ext_runtime` into `AgentRuntime::new` on a line AFTER this block, the `.clone()` here is correct; verify ordering and clone as needed.)

- [ ] **Step 4: Build + run the crate tests**

Run: `cargo build --manifest-path <WT>/Cargo.toml -p greentic-runner-host`
Run: `cargo test --manifest-path <WT>/Cargo.toml -p greentic-runner-host`
Expected: clean build; existing agent_node tests + the 3 new ones pass. (`build_agent_node_handler` itself is integration-gated on Redis + env and is not newly unit-tested; the selection branch is thin and covered by the helper tests + build.)

- [ ] **Step 5: Commit**

```bash
git -C <WT> add crates/greentic-runner-host/src/runner/agent_node.rs
git -C <WT> commit -m "feat(runner-host): run DwAgent LLM via bridge extension when configured"
```

---

### Task 2: Gates + PR

- [ ] **Step 1: Gates**

```
cargo fmt    --manifest-path <WT>/Cargo.toml -p greentic-runner-host -- --check
cargo clippy --manifest-path <WT>/Cargo.toml -p greentic-runner-host --all-targets -- -D warnings
cargo test   --manifest-path <WT>/Cargo.toml -p greentic-runner-host
```
All exit 0. (If the repo's hooks run a wider build, let them; report PRE-EXISTING unrelated failures, don't fix them.)

- [ ] **Step 2: Push + PR**

```bash
git -C <WT> push -u origin feat/llm-ext-slice3
gh pr create --base research --head feat/llm-ext-slice3 \
  --title "feat(runner-host): DwAgent LLM via bridge extension (LLM-as-extension Slice 3)" \
  --body "Slice 3 of docs/superpowers/specs/2026-05-29-llm-provider-as-extension-design.md (production-runner scope). build_agent_node_handler selects ExtensionLlmBackend (Slice 1) when GREENTIC_AW_LLM_EXTENSION names a loaded bridge extension + an LLM key is present (BridgeCredential from GREENTIC_LLM_* env); otherwise keeps the env-keyed OpenAiLlmBackend fallback. Deployed workers can now run on the bridge extension (e.g. the component-llm-openai .gtxpack installed in GREENTIC_EXTENSIONS_DIR) — no provider-specific code on the configured path. Designer playground unchanged. Operator installs the bridge extension to use it. Slice 4 = Anthropic bridge."
```
Do NOT merge (await user).

---

## Self-Review

**Spec coverage:** Slice 3 (production-runner scope) — "runner config selects the extension provider + passes the resolved credential; OpenAiLlmBackend kept as fallback" → Tasks 1–2. Designer + retiring the in-process adapter were de-scoped per the 2026-05-29 "production runner dulu" decision.

**Placeholder scan:** the `ext_runtime` clone-ordering note is a concrete verify-and-adjust instruction, not a stub. No TODO code.

**Type consistency:** `bridge_credential(...) -> Option<BridgeCredential>` matches Slice 1's `BridgeCredential{provider,model,api_key,base_url?}` exactly; `ExtensionLlmBackend::new(Arc<ExtensionRuntime>, impl Into<String>, BridgeCredential)` per Slice 1; both match arms coerce to `Arc<dyn LlmBackend>`. Env var names match the designer/runner conventions (`GREENTIC_LLM_PROVIDER/MODEL/API_KEY/BASE_URL`, `OPENAI_API_KEY` fallback) + a new `GREENTIC_AW_LLM_EXTENSION` to opt in.
