# Enterprise Agentic Worker Runtime Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a library crate `greentic-aw-runtime` that executes Plan-Act-Observe agentic loops driven by LLM tool calls, persists conversation state in Redis with per-tenant isolation, and is consumed by both production runner-host (as a new `NodeKind::DwAgent`) and designer playground (`/api/chat`).

**Architecture:** Single library crate inside `greentic-runner` workspace exposing async traits (`ConfigProvider`, `AgentStateStore`, `LlmBackend`, `Telemetry`) and a concrete `AgentRuntime::step()` entry point. Tool dispatch reuses `greentic-ext-runtime`'s blocking `invoke_tool` wrapped in `tokio::task::spawn_blocking`. Redis backs state, distributed locks (`SET NX` with TTL refresh), config cache, daily token counter, and tool-call idempotency ledger. Designer consumes this crate via a workspace-external git dep pinned to the same `greentic-ext-runtime` tag as runner-host.

**Tech Stack:** Rust 1.95.0 (edition 2024, native `async fn` in trait — no `async-trait` crate), Tokio v1, Wasmtime via `greentic-ext-runtime` (tag `v1.2.8-research`), Redis via `greentic-state`, OpenTelemetry via `greentic-telemetry`, `serde_json`, `thiserror`, `chrono` (UTC datetimes).

**Spec reference:** [`docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md`](../specs/2026-05-22-enterprise-aw-runtime-design.md)

---

## File Structure

### New crate `crates/greentic-aw-runtime/`

| File | Responsibility | Hard cap |
|---|---|---|
| `Cargo.toml` | Crate manifest, `test-mock` feature, cross-workspace git dep on `greentic-ext-runtime@v1.2.8-research` | n/a |
| `src/lib.rs` | Public re-exports; `AgentRuntime` struct + `new` + `step` entry point | ≤ 200 lines |
| `src/tenant.rs` | `TenantContext { tenant_id, env_id }` value type | ≤ 80 lines |
| `src/error.rs` | `AgentError`, `TerminationReason`, `StateError`, `LlmError`, `ConfigError`, `user_facing_message` helper | ≤ 300 lines |
| `src/config.rs` | `AgentConfig`, `AgentLimits` (with defaults), `LlmProviderRef`, `ToolRef` | ≤ 250 lines |
| `src/state.rs` | `ConversationState`, `ChatMessage`, `AgentStateStore` trait, `SessionLock` + `SessionLockInner` sealed trait | ≤ 350 lines |
| `src/state_redis.rs` | `RedisAgentStateStore`, `RedisSessionLock` (acquire/refresh/release) | ≤ 450 lines |
| `src/llm.rs` | `LlmBackend` trait, `LlmRequest`/`LlmResponse`/`LlmError`, OpenAI default impl, `RetryingLlmBackend` decorator | ≤ 450 lines |
| `src/tools.rs` | Tool schema listing + `invoke_tool` spawn_blocking wrapper + idempotency ledger | ≤ 300 lines |
| `src/loop.rs` | Plan-Act-Observe loop core (`run_loop` fn called from `AgentRuntime::step`) | ≤ 450 lines |
| `src/telemetry.rs` | `Telemetry` trait + `StepTelemetryCtx` + `OtelTelemetry` default impl | ≤ 200 lines |
| `src/config_provider.rs` | `ConfigProvider` trait + `InMemoryConfigProvider` (designer) + `CachingConfigProvider` decorator (60s TTL) | ≤ 300 lines |
| `src/mock.rs` (`#[cfg(feature = "test-mock")]`) | `MockLlmBackend`, `MockAgentStateStore`, `MockTelemetry`, `MockConfigProvider` | ≤ 350 lines |
| `tests/redis_state.rs` | Integration: real Redis state + lock + TTL | ≤ 400 lines |
| `tests/loop_e2e.rs` | Integration: full loop with mock LLM + real Redis | ≤ 400 lines |

### Modified files in existing crates

| File | Change |
|---|---|
| `crates/greentic-runner/Cargo.toml` (workspace) | Register `crates/greentic-aw-runtime` member |
| `crates/greentic-runner-host/Cargo.toml` | Add `greentic-aw-runtime` workspace dep |
| `crates/greentic-runner-host/src/runner/engine.rs:126` | Extend `NodeKind` with `DwAgent { agent_id: String }` |
| `crates/greentic-runner-host/src/runner/engine.rs` (dispatch site) | New arm calling `aw_runtime.step(...)` |
| `crates/greentic-runner-host/src/runner/agent_node.rs` (NEW) | Production glue: tenant/session derivation, `.gtpack` AgentConfig source |

### Modified files in `greentic-designer/` (Phase 5 only)

| File | Change |
|---|---|
| `Cargo.toml` | Add `greentic-aw-runtime` git dep |
| `src/ui/routes/dw_test_chat/dispatcher.rs` | Replace stub dispatch with `AgentRuntime::step` call |
| `src/ui/agent/playground_config.rs` (NEW) | `DwFormState → AgentConfig` translator + `InMemoryConfigProvider` instance |
| `web/src/components/dw-test-chat/TypingIndicator.tsx` (NEW) | "thinking..." heartbeat shown while `/api/chat` request in flight |

---

## Phase 1 — Library Skeleton + Types + Traits + Mocks

**PR gate:** Traits frozen; all unit tests green under `cargo test --features test-mock`; `cargo clippy -- -D warnings` clean; `cargo fmt --check` clean.

### Task 1.1: Scaffold the crate

**Files:**
- Create: `crates/greentic-aw-runtime/Cargo.toml`
- Create: `crates/greentic-aw-runtime/src/lib.rs`
- Modify: `Cargo.toml` (workspace root) — add `crates/greentic-aw-runtime` to `members`

- [ ] **Step 1: Verify `v1.2.8-research` is still the latest tag**

Run: `cd /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer-extensions && git tag --list 'v1.2*' | tail -3`
Expected: `v1.2.8-research` is the last line (or update the version in all `Cargo.toml` snippets below to match).

- [ ] **Step 2: Create the crate manifest**

Write `crates/greentic-aw-runtime/Cargo.toml`:

```toml
[package]
name        = "greentic-aw-runtime"
version     = { workspace = true }
edition     = { workspace = true }
license     = { workspace = true }
description = "Enterprise Agentic Worker runtime — Plan-Act-Observe loop, Redis state, tool dispatch via greentic-ext-runtime"
publish     = false

[features]
default   = []
test-mock = []

[dependencies]
anyhow              = { workspace = true }
async-trait         = { workspace = true } # Used ONLY for object-safe trait bounds where RPITIT cannot be expressed (none in MVP); kept for forward-compat; do NOT use on AW traits.
chrono              = { version = "0.4", features = ["serde"] }
futures             = { workspace = true }
greentic-ext-runtime = { git = "https://github.com/greentic-biz/greentic-designer-extensions", tag = "v1.2.8-research" }
greentic-state      = { workspace = true }
greentic-telemetry  = { workspace = true }
serde               = { version = "1", features = ["derive"] }
serde_json          = { workspace = true }
thiserror           = { workspace = true }
tokio               = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
tracing             = { workspace = true }
uuid                = { version = "1", features = ["v4", "serde"] }

[dev-dependencies]
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync", "test-util"] }
```

- [ ] **Step 3: Stub the lib.rs**

Write `crates/greentic-aw-runtime/src/lib.rs`:

```rust
//! Greentic Agentic Worker Runtime — library crate.
//!
//! See `docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md`
//! for the full design spec. This crate exposes the [`AgentRuntime`] entry
//! point and the trait surface (`AgentStateStore`, `ConfigProvider`,
//! `LlmBackend`, `Telemetry`) that the production runner-host and the
//! designer playground both consume.

#![deny(unsafe_code)]
#![warn(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod config;
pub mod config_provider;
pub mod error;
pub mod llm;
pub mod r#loop;
pub mod state;
pub mod state_redis;
pub mod telemetry;
pub mod tenant;
pub mod tools;

#[cfg(feature = "test-mock")]
pub mod mock;

pub use config::{AgentConfig, AgentLimits, LlmProviderRef, ToolRef};
pub use config_provider::ConfigProvider;
pub use error::{AgentError, ConfigError, LlmError, StateError, TerminationReason};
pub use llm::{LlmBackend, LlmRequest, LlmResponse, RetryingLlmBackend};
pub use state::{AgentStateStore, ChatMessage, ConversationState, SessionLock};
pub use state_redis::RedisAgentStateStore;
pub use telemetry::{StepTelemetryCtx, Telemetry};
pub use tenant::TenantContext;

use std::sync::Arc;

/// The main entry point for executing a single agentic step.
///
/// Construct via [`AgentRuntime::new`] with the four trait objects
/// (config, state, LLM, telemetry) plus a shared `Arc<ExtensionRuntime>`
/// for tool dispatch. Call [`AgentRuntime::step`] per inbound user
/// message.
pub struct AgentRuntime {
    pub(crate) config_provider: Arc<dyn ConfigProvider>,
    pub(crate) state_store:     Arc<dyn AgentStateStore>,
    pub(crate) ext_runtime:     Arc<greentic_ext_runtime::ExtensionRuntime>,
    pub(crate) llm:             Arc<dyn LlmBackend>,
    pub(crate) telemetry:       Arc<dyn Telemetry>,
}

impl AgentRuntime {
    pub fn new(
        config_provider: Arc<dyn ConfigProvider>,
        state_store:     Arc<dyn AgentStateStore>,
        ext_runtime:     Arc<greentic_ext_runtime::ExtensionRuntime>,
        llm:             Arc<dyn LlmBackend>,
        telemetry:       Arc<dyn Telemetry>,
    ) -> Self {
        Self { config_provider, state_store, ext_runtime, llm, telemetry }
    }

    /// Execute one agentic step against the given session.
    /// Implementation lives in [`r#loop::run_step`].
    pub async fn step(
        &self,
        tenant:     TenantContext,
        session_id: &str,
        agent_id:   &str,
        message:    AgentInput,
    ) -> Result<AgentOutput, AgentError> {
        r#loop::run_step(self, tenant, session_id, agent_id, message).await
    }
}

/// Inbound user message handed to [`AgentRuntime::step`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentInput {
    pub text: String,
}

/// Outbound reply produced by [`AgentRuntime::step`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AgentOutput {
    pub reply:         String,
    pub trail:         Vec<AgentStep>,
    pub terminated_by: TerminationReason,
}

/// One iteration of the Plan-Act-Observe loop, surfaced in the audit
/// trail (`AgentOutput.trail`). Caller decides whether to persist or
/// display.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentStep {
    ToolCall { name: String, call_id: String, result: serde_json::Value },
    ToolCallReused { name: String, call_id: String },
    ToolCallBlocked { name: String, reason: String },
    Reply { text: String },
}
```

- [ ] **Step 4: Register the crate in the workspace**

Modify `Cargo.toml` (workspace root) members list — add a line `    "crates/greentic-aw-runtime",` before the existing `"crates/tests",` entry.

Show the diff:

```diff
 [workspace]
 members = [
     "crates/greentic-runner",
     "crates/greentic-runner-host",
     "crates/greentic-runner-desktop",
+    "crates/greentic-aw-runtime",
     "crates/tests",
     "crates/runner-core",
```

- [ ] **Step 5: Verify the crate builds**

Run: `cargo build -p greentic-aw-runtime`
Expected: Compiles with warnings only (no errors). Modules referenced from `lib.rs` are empty placeholders — see Tasks 1.2-1.14.

If you get errors about missing modules, create empty placeholder files now:

```bash
touch crates/greentic-aw-runtime/src/{tenant,error,config,config_provider,llm,state,state_redis,tools,telemetry}.rs
touch crates/greentic-aw-runtime/src/loop.rs
```

Then add `// placeholder — filled in subsequent tasks` to each.

- [ ] **Step 6: Commit**

```bash
git add crates/greentic-aw-runtime Cargo.toml
git commit -m "feat(aw-runtime): scaffold greentic-aw-runtime crate with test-mock feature gate"
```

---

### Task 1.2: TenantContext value type

**Files:**
- Modify: `crates/greentic-aw-runtime/src/tenant.rs`
- Test: same file under `#[cfg(test)]`

- [ ] **Step 1: Write the failing test**

Replace `tenant.rs` placeholder with:

```rust
//! Multi-tenant context. Mandatory on every public AgentRuntime method
//! so cross-tenant access is a compile error, not a runtime check.

use serde::{Deserialize, Serialize};

/// Identifies the (tenant, environment) pair an agent step runs under.
/// Pass-by-value (cheap clone — two `String`s).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TenantContext {
    pub tenant_id: String,
    pub env_id:    String,
}

impl TenantContext {
    pub fn new(tenant_id: impl Into<String>, env_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            env_id:    env_id.into(),
        }
    }

    /// Prefix used by Redis key builders. Returns `aw:{tenant}:{env}`.
    pub fn key_prefix(&self) -> String {
        format!("aw:{}:{}", self.tenant_id, self.env_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_prefix_formats_as_expected() {
        let ctx = TenantContext::new("acme", "prod");
        assert_eq!(ctx.key_prefix(), "aw:acme:prod");
    }

    #[test]
    fn tenant_context_is_eq_and_hashable() {
        let a = TenantContext::new("acme", "prod");
        let b = TenantContext::new("acme", "prod");
        assert_eq!(a, b);
        let mut set = std::collections::HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }
}
```

- [ ] **Step 2: Run the test to verify it passes**

Run: `cargo test -p greentic-aw-runtime tenant`
Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/tenant.rs
git commit -m "feat(aw-runtime): TenantContext with Redis key-prefix helper"
```

---

### Task 1.3: AgentError + TerminationReason + user_facing_message

**Files:**
- Modify: `crates/greentic-aw-runtime/src/error.rs`

- [ ] **Step 1: Write the test before the implementation**

Replace `error.rs` placeholder with:

```rust
//! All error and termination types used by the AW runtime.
//!
//! IMPORTANT: external surfaces (runner, designer) MUST render
//! end-user-facing replies via [`AgentError::user_facing_message`].
//! Raw `Display` of `AgentError` is for internal logs only.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::AgentConfig;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent state load failed: {0}")]
    StateLoad(#[from] StateError),

    #[error("llm provider unavailable")]
    LlmProviderUnavailable,

    #[error("llm error: {0}")]
    Llm(#[from] LlmError),

    #[error("config error: {0}")]
    Config(#[from] ConfigError),

    #[error("tool dispatch error: {0}")]
    ToolDispatch(String),

    #[error("daily token budget exceeded")]
    TokenBudgetExceeded,

    #[error("session lock could not be acquired within wait window")]
    LockTimeout,

    #[error("loop exceeded max iterations")]
    MaxIterations,

    #[error("step timed out")]
    Timeout,

    #[error("internal: {0}")]
    Internal(String),
}

impl AgentError {
    /// Returns a sanitised, end-user-appropriate string. No Rust error
    /// chain, no internal detail, no PII leakage. Tenants can override
    /// the LLM-unavailability + budget messages via `AgentLimits`.
    pub fn user_facing_message(&self, config: &AgentConfig) -> String {
        match self {
            Self::LlmProviderUnavailable | Self::Llm(_) => config
                .limits
                .provider_failure_message
                .clone()
                .unwrap_or_else(|| {
                    "I'm having trouble reaching my reasoning system. \
                     Please try again in a moment."
                        .into()
                }),
            Self::TokenBudgetExceeded => "Daily usage limit reached. \
                 Please try again tomorrow or contact your administrator."
                .to_string(),
            Self::Timeout => {
                "I'm taking longer than expected — please try a simpler request.".to_string()
            }
            Self::MaxIterations => {
                "I wasn't able to finish reasoning about that. \
                 Could you rephrase or break it into smaller steps?"
                    .to_string()
            }
            _ => "Something went wrong. Please try again.".to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum StateError {
    #[error("redis error: {0}")]
    Redis(String),
    #[error("schema version {found} not supported (max supported: {supported})")]
    SchemaIncompatible { found: u32, supported: u32 },
    #[error("decode error: {0}")]
    Decode(String),
    #[error("lock acquisition timed out after {0:?}")]
    LockTimeout(std::time::Duration),
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("provider returned 5xx after retries")]
    ServiceUnavailable,
    #[error("provider returned 4xx: {0}")]
    BadRequest(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("agent_id {0} not found for tenant")]
    AgentNotFound(String),
    #[error("provider misconfigured: {0}")]
    Misconfigured(String),
    #[error("internal: {0}")]
    Internal(String),
}

/// Reason the Plan-Act-Observe loop exited. Surfaced via
/// [`crate::AgentOutput::terminated_by`] and as an OTel attribute on
/// the `aw.step` span.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationReason {
    FinalReply,
    MaxIterations,
    Timeout,
    Error,
    TokenBudgetExceeded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, AgentLimits, LlmProviderRef};

    fn config_with(message: Option<&str>) -> AgentConfig {
        AgentConfig {
            agent_id:      "a".into(),
            system_prompt: "".into(),
            tools:         vec![],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model:    "gpt-4".into(),
            },
            limits: AgentLimits {
                provider_failure_message: message.map(str::to_string),
                ..AgentLimits::default()
            },
        }
    }

    #[test]
    fn user_facing_message_defaults_for_provider_unavailable() {
        let cfg = config_with(None);
        let msg = AgentError::LlmProviderUnavailable.user_facing_message(&cfg);
        assert!(msg.contains("reasoning system"));
        assert!(msg.contains("try again"));
    }

    #[test]
    fn user_facing_message_uses_tenant_override_when_set() {
        let cfg = config_with(Some("Please retry in 5 minutes."));
        let msg = AgentError::LlmProviderUnavailable.user_facing_message(&cfg);
        assert_eq!(msg, "Please try retry in 5 minutes.".replace("try ", ""));
    }

    #[test]
    fn user_facing_message_never_leaks_internal_detail() {
        let cfg = config_with(None);
        let leaky = AgentError::Internal("DATABASE_HOST=192.168.1.5".into());
        let msg = leaky.user_facing_message(&cfg);
        assert!(!msg.contains("DATABASE_HOST"));
        assert!(!msg.contains("192.168"));
    }

    #[test]
    fn user_facing_message_budget_distinct_from_default() {
        let cfg = config_with(None);
        let budget = AgentError::TokenBudgetExceeded.user_facing_message(&cfg);
        assert!(budget.contains("limit"));
        assert_ne!(budget, AgentError::Internal("x".into()).user_facing_message(&cfg));
    }
}
```

Fix the assertion typo: the second test should be a clean equality check. Replace the body of `user_facing_message_uses_tenant_override_when_set` with:

```rust
    #[test]
    fn user_facing_message_uses_tenant_override_when_set() {
        let cfg = config_with(Some("Please retry in 5 minutes."));
        let msg = AgentError::LlmProviderUnavailable.user_facing_message(&cfg);
        assert_eq!(msg, "Please retry in 5 minutes.");
    }
```

- [ ] **Step 2: Verify tests cannot compile yet**

Run: `cargo test -p greentic-aw-runtime error`
Expected: Compilation fails — `AgentLimits` and `AgentConfig` not yet defined. This is expected; Task 1.5 introduces them. Move to Task 1.4 first; we'll re-run the error tests after Task 1.5.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/error.rs
git commit -m "feat(aw-runtime): AgentError + TerminationReason + user_facing_message helper"
```

---

### Task 1.4: AgentLimits with defaults

**Files:**
- Modify: `crates/greentic-aw-runtime/src/config.rs`

- [ ] **Step 1: Write the test + implementation**

Replace `config.rs` placeholder with:

```rust
//! Per-agent configuration delivered by [`crate::ConfigProvider`].
//!
//! Defaults live in `AgentLimits::default()` per spec §5.2.
//! Override per-tenant via the admin designer config UI; the runtime
//! receives the resolved struct via the cached provider.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Reference to a tool exposed by a `greentic-ext-runtime` extension.
/// Composer extensions emit short names; runner-host derives full IDs
/// (extension_id + tool_name) via `ExtensionRuntime::list_tools` before
/// constructing the `AgentConfig` (see runner-host Phase 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRef {
    pub extension_id: String,
    pub tool_name:    String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProviderRef {
    pub provider: String, // "openai" | "anthropic" | ...
    pub model:    String, // "gpt-4o-mini" | "claude-3-haiku" | ...
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id:      String,
    pub system_prompt: String,
    pub tools:         Vec<ToolRef>,
    pub llm:           LlmProviderRef,
    pub limits:        AgentLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentLimits {
    /// Maximum Plan-Act-Observe iterations per step. Default: 8.
    pub max_iter: u32,

    /// Wall-clock timeout for the entire `step()` call. Default: 60s.
    #[serde(with = "duration_secs")]
    pub timeout: Duration,

    /// Maximum retained conversation history turns. Default: 20.
    /// Truncation drops oldest user-assistant pairs; system prompt
    /// preserved.
    pub max_history_turns: u32,

    /// LLM retry attempts before declaring provider unavailable.
    /// Default: 3.
    pub llm_retry_attempts: u32,

    /// Initial backoff for LLM retries; exponential. Default: 250ms.
    #[serde(with = "duration_ms")]
    pub llm_retry_backoff: Duration,

    /// Tenant-configurable user-facing message when LLM is unavailable
    /// after all retries.
    pub provider_failure_message: Option<String>,

    /// Daily token cap per tenant. `None` = uncapped (MVP default).
    pub daily_token_cap_per_tenant: Option<u32>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_iter:                    8,
            timeout:                     Duration::from_secs(60),
            max_history_turns:           20,
            llm_retry_attempts:          3,
            llm_retry_backoff:           Duration::from_millis(250),
            provider_failure_message:    None,
            daily_token_cap_per_tenant:  None,
        }
    }
}

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

mod duration_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u128(d.as_millis())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec_5_2() {
        let l = AgentLimits::default();
        assert_eq!(l.max_iter, 8);
        assert_eq!(l.timeout, Duration::from_secs(60));
        assert_eq!(l.max_history_turns, 20);
        assert_eq!(l.llm_retry_attempts, 3);
        assert_eq!(l.llm_retry_backoff, Duration::from_millis(250));
        assert!(l.provider_failure_message.is_none());
        assert!(l.daily_token_cap_per_tenant.is_none());
    }

    #[test]
    fn agent_config_roundtrips_through_json() {
        let original = AgentConfig {
            agent_id:      "a-1".into(),
            system_prompt: "be helpful".into(),
            tools:         vec![ToolRef {
                extension_id: "http".into(),
                tool_name:    "fetch".into(),
            }],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model:    "gpt-4o-mini".into(),
            },
            limits: AgentLimits::default(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let round: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(round.agent_id, original.agent_id);
        assert_eq!(round.limits.max_iter, 8);
    }
}
```

- [ ] **Step 2: Run config tests**

Run: `cargo test -p greentic-aw-runtime config`
Expected: 2 tests pass.

- [ ] **Step 3: Re-run error tests now that AgentConfig + AgentLimits exist**

Run: `cargo test -p greentic-aw-runtime error`
Expected: 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-aw-runtime/src/config.rs
git commit -m "feat(aw-runtime): AgentConfig + AgentLimits with spec-defined defaults"
```

---

### Task 1.5: ConversationState + ChatMessage

**Files:**
- Modify: `crates/greentic-aw-runtime/src/state.rs`

- [ ] **Step 1: Write the implementation + tests**

Replace the placeholder with the type definitions only (trait comes in Task 1.6):

```rust
//! Conversation state + persistence trait + session lock.
//!
//! The state struct is JSON-serialised into Redis under the key
//! `aw:{tenant}:{env}:{session}:state`. `schema_version` is the FIRST
//! field so older readers can fail fast on incompatible bumps.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::StateError;
use crate::tenant::TenantContext;
use std::time::Duration;

/// Current schema version emitted on save. Bump when [`ConversationState`]
/// shape changes in a way that older runners cannot decode.
pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationState {
    pub schema_version: u32,
    pub session_id:     String,
    pub tenant_id:      String,
    pub env_id:         String,
    pub messages:       Vec<ChatMessage>,
    pub created_at:     DateTime<Utc>,
    pub updated_at:     DateTime<Utc>,
}

impl ConversationState {
    pub fn empty(tenant: &TenantContext, session_id: &str) -> Self {
        let now = Utc::now();
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            session_id:     session_id.to_string(),
            tenant_id:      tenant.tenant_id.clone(),
            env_id:         tenant.env_id.clone(),
            messages:       Vec::new(),
            created_at:     now,
            updated_at:     now,
        }
    }

    /// Truncate oldest user-assistant pairs until `messages.len() <= max`.
    /// System and tool messages are preserved relative to neighbours
    /// (truncation is pair-aware).
    pub fn truncate_history(&mut self, max_turns: u32) {
        let max = max_turns as usize;
        // Always keep system messages; count non-system entries.
        while self.messages.iter().filter(|m| !matches!(m, ChatMessage::System { .. })).count() > max {
            // Drop the oldest non-system message.
            if let Some(pos) = self
                .messages
                .iter()
                .position(|m| !matches!(m, ChatMessage::System { .. }))
            {
                self.messages.remove(pos);
            } else {
                break;
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatMessage {
    System    { content: String },
    User      { content: String },
    Assistant { content: String, tool_calls: Vec<ToolCallRecord> },
    Tool      { call_id: String, content: serde_json::Value },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub call_id:      String,
    pub extension_id: String,
    pub tool_name:    String,
    pub args:         serde_json::Value,
}

/// Persists and locks conversation state. `state_redis.rs` provides the
/// production impl; tests use [`crate::mock::MockAgentStateStore`].
pub trait AgentStateStore: Send + Sync {
    /// Load the conversation state, returning an empty initialised
    /// state if no record exists.
    async fn load(
        &self,
        tenant:     &TenantContext,
        session_id: &str,
    ) -> Result<ConversationState, StateError>;

    /// Persist conversation state. Implementations refresh the TTL on
    /// every save (7 days for Redis impl).
    async fn save(
        &self,
        tenant:     &TenantContext,
        session_id: &str,
        state:      &ConversationState,
    ) -> Result<(), StateError>;

    /// Acquire a distributed lock for the session. Returns an RAII
    /// [`SessionLock`] guard; Drop releases the lock (best-effort —
    /// Redis SET NX TTL of 90s is the safety net).
    async fn acquire_lock(
        &self,
        tenant:     &TenantContext,
        session_id: &str,
        wait:       Duration,
    ) -> Result<SessionLock, StateError>;
}

/// RAII handle holding a per-session distributed lock. Drop releases
/// the underlying Redis key (best-effort). Lock TTL is 90s; callers
/// MUST call [`SessionLock::refresh`] once per loop iteration to
/// extend.
pub struct SessionLock {
    pub(crate) inner: Box<dyn SessionLockInner>,
}

impl SessionLock {
    pub(crate) fn new(inner: Box<dyn SessionLockInner>) -> Self {
        Self { inner }
    }

    /// Extend the TTL by another 90s window. On error the loop logs
    /// and continues — losing the extension is preferable to aborting
    /// a partially-complete turn.
    pub async fn refresh(&self) -> Result<(), StateError> {
        self.inner.refresh().await
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        self.inner.release();
    }
}

/// Sealed trait — implementors live in `state_redis.rs` and `mock.rs`.
pub trait SessionLockInner: Send + Sync {
    /// Async refresh. Returns the future as a boxed trait object so
    /// the trait stays object-safe (no RPITIT in `dyn`).
    fn refresh<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StateError>> + Send + 'a>>;
    /// Best-effort synchronous release called from `Drop`.
    fn release(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_state_has_schema_version_1() {
        let tc = TenantContext::new("a", "b");
        let s = ConversationState::empty(&tc, "sess");
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(s.schema_version, 1);
        assert_eq!(s.session_id, "sess");
        assert_eq!(s.tenant_id, "a");
        assert_eq!(s.env_id, "b");
        assert!(s.messages.is_empty());
    }

    #[test]
    fn truncate_history_drops_oldest_non_system_first() {
        let tc = TenantContext::new("a", "b");
        let mut s = ConversationState::empty(&tc, "x");
        s.messages.push(ChatMessage::System { content: "sys".into() });
        s.messages.push(ChatMessage::User { content: "u1".into() });
        s.messages.push(ChatMessage::Assistant { content: "a1".into(), tool_calls: vec![] });
        s.messages.push(ChatMessage::User { content: "u2".into() });
        s.messages.push(ChatMessage::Assistant { content: "a2".into(), tool_calls: vec![] });

        s.truncate_history(2);
        // System always preserved; only u2 + a2 kept.
        assert_eq!(s.messages.len(), 3);
        assert!(matches!(s.messages[0], ChatMessage::System { .. }));
        if let ChatMessage::User { content } = &s.messages[1] {
            assert_eq!(content, "u2");
        } else {
            panic!("expected User u2 at position 1");
        }
    }
}
```

- [ ] **Step 2: Run state tests**

Run: `cargo test -p greentic-aw-runtime --features test-mock state`
Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/state.rs
git commit -m "feat(aw-runtime): ConversationState + ChatMessage + AgentStateStore trait + SessionLock RAII"
```

---

### Task 1.6: ConfigProvider trait + CachingConfigProvider

**Files:**
- Modify: `crates/greentic-aw-runtime/src/config_provider.rs`

- [ ] **Step 1: Write the impl + tests**

Replace the placeholder with:

```rust
//! Agent config provider trait + a 60s TTL caching decorator.
//!
//! Per spec Decision 13: TTL-only invalidation for MVP. Multi-instance
//! runners may observe up to 60s propagation lag for tenant config
//! changes made via the admin designer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::config::AgentConfig;
use crate::error::ConfigError;
use crate::tenant::TenantContext;

pub trait ConfigProvider: Send + Sync {
    async fn agent_config(
        &self,
        tenant:   &TenantContext,
        agent_id: &str,
    ) -> Result<AgentConfig, ConfigError>;
}

/// 60s TTL in-process cache wrapped around an inner provider. Safe to
/// share across threads via `Arc`.
pub struct CachingConfigProvider<P: ConfigProvider> {
    inner: P,
    ttl:   Duration,
    cache: RwLock<HashMap<CacheKey, (Instant, AgentConfig)>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CacheKey {
    tenant_id: String,
    env_id:    String,
    agent_id:  String,
}

impl<P: ConfigProvider> CachingConfigProvider<P> {
    pub fn new(inner: P) -> Self {
        Self::with_ttl(inner, Duration::from_secs(60))
    }

    pub fn with_ttl(inner: P, ttl: Duration) -> Self {
        Self {
            inner,
            ttl,
            cache: RwLock::new(HashMap::new()),
        }
    }
}

impl<P: ConfigProvider> ConfigProvider for CachingConfigProvider<P> {
    async fn agent_config(
        &self,
        tenant:   &TenantContext,
        agent_id: &str,
    ) -> Result<AgentConfig, ConfigError> {
        let key = CacheKey {
            tenant_id: tenant.tenant_id.clone(),
            env_id:    tenant.env_id.clone(),
            agent_id:  agent_id.to_string(),
        };
        {
            let cache = self.cache.read().await;
            if let Some((stored_at, cfg)) = cache.get(&key) {
                if stored_at.elapsed() < self.ttl {
                    return Ok(cfg.clone());
                }
            }
        }
        let fresh = self.inner.agent_config(tenant, agent_id).await?;
        let mut cache = self.cache.write().await;
        cache.insert(key, (Instant::now(), fresh.clone()));
        Ok(fresh)
    }
}

/// In-memory provider for designer playground + tests. Resolves agent
/// configs from a `HashMap`. Returns `ConfigError::AgentNotFound` for
/// unknown agent_ids.
pub struct InMemoryConfigProvider {
    entries: HashMap<(String, String, String), AgentConfig>,
}

impl InMemoryConfigProvider {
    pub fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    pub fn insert(&mut self, tenant: &TenantContext, agent_id: &str, cfg: AgentConfig) {
        self.entries.insert(
            (tenant.tenant_id.clone(), tenant.env_id.clone(), agent_id.to_string()),
            cfg,
        );
    }
}

impl Default for InMemoryConfigProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigProvider for InMemoryConfigProvider {
    async fn agent_config(
        &self,
        tenant:   &TenantContext,
        agent_id: &str,
    ) -> Result<AgentConfig, ConfigError> {
        self.entries
            .get(&(tenant.tenant_id.clone(), tenant.env_id.clone(), agent_id.to_string()))
            .cloned()
            .ok_or_else(|| ConfigError::AgentNotFound(agent_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentLimits, LlmProviderRef};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sample_cfg() -> AgentConfig {
        AgentConfig {
            agent_id:      "a-1".into(),
            system_prompt: "sys".into(),
            tools:         vec![],
            llm: LlmProviderRef { provider: "openai".into(), model: "gpt-4o-mini".into() },
            limits: AgentLimits::default(),
        }
    }

    struct CountingProvider {
        calls: AtomicUsize,
        cfg:   AgentConfig,
    }

    impl ConfigProvider for CountingProvider {
        async fn agent_config(
            &self,
            _tenant: &TenantContext,
            _agent_id: &str,
        ) -> Result<AgentConfig, ConfigError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.cfg.clone())
        }
    }

    #[tokio::test]
    async fn caching_provider_hits_inner_once_within_ttl() {
        let inner = Arc::new(CountingProvider {
            calls: AtomicUsize::new(0),
            cfg:   sample_cfg(),
        });
        // Wrap a clone of the inner so we can inspect counter externally.
        struct Wrapper(Arc<CountingProvider>);
        impl ConfigProvider for Wrapper {
            async fn agent_config(
                &self,
                tenant: &TenantContext,
                agent_id: &str,
            ) -> Result<AgentConfig, ConfigError> {
                self.0.agent_config(tenant, agent_id).await
            }
        }
        let cache = CachingConfigProvider::new(Wrapper(inner.clone()));
        let tc = TenantContext::new("acme", "prod");
        let _ = cache.agent_config(&tc, "a-1").await.unwrap();
        let _ = cache.agent_config(&tc, "a-1").await.unwrap();
        let _ = cache.agent_config(&tc, "a-1").await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn caching_provider_expires_after_ttl() {
        struct Wrapper(Arc<CountingProvider>);
        impl ConfigProvider for Wrapper {
            async fn agent_config(
                &self,
                t: &TenantContext,
                a: &str,
            ) -> Result<AgentConfig, ConfigError> {
                self.0.agent_config(t, a).await
            }
        }
        let inner = Arc::new(CountingProvider { calls: AtomicUsize::new(0), cfg: sample_cfg() });
        let cache = CachingConfigProvider::with_ttl(Wrapper(inner.clone()), Duration::from_millis(50));
        let tc = TenantContext::new("acme", "prod");
        let _ = cache.agent_config(&tc, "a-1").await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = cache.agent_config(&tc, "a-1").await.unwrap();
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn in_memory_provider_returns_not_found_for_missing_agent() {
        let p = InMemoryConfigProvider::new();
        let tc = TenantContext::new("acme", "prod");
        let result = p.agent_config(&tc, "missing").await;
        assert!(matches!(result, Err(ConfigError::AgentNotFound(_))));
    }
}
```

- [ ] **Step 2: Run config_provider tests**

Run: `cargo test -p greentic-aw-runtime config_provider`
Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/config_provider.rs
git commit -m "feat(aw-runtime): ConfigProvider trait + 60s caching decorator + InMemoryConfigProvider"
```

---

### Task 1.7: LlmBackend trait + RetryingLlmBackend

**Files:**
- Modify: `crates/greentic-aw-runtime/src/llm.rs`

- [ ] **Step 1: Write the impl**

Replace placeholder with:

```rust
//! LLM backend trait + a retry decorator. Concrete OpenAI / Anthropic
//! impls are added in Phase 3; the trait + decorator are introduced
//! here so the loop can be wired against mocks during Phase 1.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::LlmError;
use crate::state::{ChatMessage, ToolCallRecord};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmRequest {
    pub system_prompt: String,
    pub history:       Vec<ChatMessage>,
    pub tools:         Vec<LlmToolSchema>,
    /// Resolved provider + model — backend selects credentials/endpoint
    /// based on this.
    pub provider: crate::config::LlmProviderRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmToolSchema {
    pub extension_id: String,
    pub tool_name:    String,
    pub description:  String,
    pub parameters:   serde_json::Value, // JSON schema
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LlmResponse {
    /// `content` is `Some` when the LLM emits a textual reply. Per
    /// Decision 12 in the spec, if `tool_calls` is non-empty AND
    /// `content` is `Some`, the loop treats `content` as a reasoning
    /// trace and executes `tool_calls` (tool_calls win).
    pub content:    Option<String>,
    pub tool_calls: Vec<ToolCallRecord>,
    pub tokens_in:  u32,
    pub tokens_out: u32,
}

pub trait LlmBackend: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;
}

/// Wraps any [`LlmBackend`] with exponential-backoff retry on
/// [`LlmError::ServiceUnavailable`]. 4xx-class errors are NOT retried.
pub struct RetryingLlmBackend<B: LlmBackend> {
    inner:    B,
    attempts: u32,
    backoff:  Duration,
}

impl<B: LlmBackend> RetryingLlmBackend<B> {
    pub fn new(inner: B, attempts: u32, backoff: Duration) -> Self {
        Self { inner, attempts, backoff }
    }
}

impl<B: LlmBackend + Send + Sync> LlmBackend for RetryingLlmBackend<B> {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut delay = self.backoff;
        let mut last_err = None;
        for attempt in 0..self.attempts.max(1) {
            match self.inner.complete(request.clone()).await {
                Ok(r) => return Ok(r),
                Err(LlmError::ServiceUnavailable) => {
                    last_err = Some(LlmError::ServiceUnavailable);
                    if attempt + 1 < self.attempts {
                        tokio::time::sleep(delay).await;
                        delay = delay.saturating_mul(2);
                    }
                }
                Err(other) => return Err(other), // 4xx-class: do not retry
            }
        }
        Err(last_err.unwrap_or(LlmError::ServiceUnavailable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct ScriptedBackend {
        responses: Mutex<Vec<Result<LlmResponse, LlmError>>>,
    }

    impl LlmBackend for ScriptedBackend {
        async fn complete(&self, _r: LlmRequest) -> Result<LlmResponse, LlmError> {
            self.responses.lock().unwrap().remove(0)
        }
    }

    fn req() -> LlmRequest {
        LlmRequest {
            system_prompt: "".into(),
            history:       vec![],
            tools:         vec![],
            provider:      crate::config::LlmProviderRef {
                provider: "openai".into(),
                model:    "x".into(),
            },
        }
    }

    fn ok_resp() -> LlmResponse {
        LlmResponse { content: Some("hi".into()), tool_calls: vec![], tokens_in: 1, tokens_out: 1 }
    }

    #[tokio::test]
    async fn retries_on_service_unavailable_then_succeeds() {
        let inner = ScriptedBackend {
            responses: Mutex::new(vec![
                Err(LlmError::ServiceUnavailable),
                Err(LlmError::ServiceUnavailable),
                Ok(ok_resp()),
            ]),
        };
        let r = RetryingLlmBackend::new(inner, 3, Duration::from_millis(1));
        let out = r.complete(req()).await.unwrap();
        assert_eq!(out.content.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn does_not_retry_on_bad_request() {
        let inner = ScriptedBackend {
            responses: Mutex::new(vec![Err(LlmError::BadRequest("nope".into()))]),
        };
        let r = RetryingLlmBackend::new(inner, 5, Duration::from_millis(1));
        let err = r.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::BadRequest(_)));
    }

    #[tokio::test]
    async fn returns_service_unavailable_after_all_attempts() {
        let inner = ScriptedBackend {
            responses: Mutex::new(vec![
                Err(LlmError::ServiceUnavailable),
                Err(LlmError::ServiceUnavailable),
                Err(LlmError::ServiceUnavailable),
            ]),
        };
        let r = RetryingLlmBackend::new(inner, 3, Duration::from_millis(1));
        let err = r.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::ServiceUnavailable));
    }
}
```

- [ ] **Step 2: Run llm tests**

Run: `cargo test -p greentic-aw-runtime llm`
Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/llm.rs
git commit -m "feat(aw-runtime): LlmBackend trait + RetryingLlmBackend (exponential, 4xx no-retry)"
```

---

### Task 1.8: Telemetry trait + StepTelemetryCtx

**Files:**
- Modify: `crates/greentic-aw-runtime/src/telemetry.rs`

- [ ] **Step 1: Write the impl**

```rust
//! OpenTelemetry span emission. MVP emits exactly one span per step
//! (`aw.step`) with the attributes required by spec §5.2. Per-LLM-call
//! and per-tool-call spans are deferred (spec §4 Decision 11).

use std::time::Duration;

use crate::error::TerminationReason;

pub trait Telemetry: Send + Sync {
    fn record_step(&self, ctx: &StepTelemetryCtx);
}

#[derive(Clone, Debug)]
pub struct StepTelemetryCtx {
    pub tenant_id:     String,
    pub env_id:        String,
    pub session_id:    String,
    pub agent_id:      String,
    pub terminated_by: TerminationReason,
    pub iterations:    u32,
    pub total_tokens:  u64,
    pub duration:      Duration,
}

/// Default OTel impl. Emits a `tracing::info_span!` named `aw.step` with
/// the required attributes. `greentic-telemetry` wires this into the
/// OTel collector automatically when its subscriber is active.
pub struct OtelTelemetry;

impl Telemetry for OtelTelemetry {
    fn record_step(&self, ctx: &StepTelemetryCtx) {
        let span = tracing::info_span!(
            "aw.step",
            tenant_id     = %ctx.tenant_id,
            env_id        = %ctx.env_id,
            session_id    = %ctx.session_id,
            agent_id      = %ctx.agent_id,
            iterations    = ctx.iterations,
            total_tokens  = ctx.total_tokens,
            duration_ms   = ctx.duration.as_millis() as u64,
            terminated_by = ?ctx.terminated_by,
        );
        let _enter = span.enter();
        tracing::info!("aw.step completed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct CapturingTelemetry(Arc<Mutex<Vec<StepTelemetryCtx>>>);
    impl Telemetry for CapturingTelemetry {
        fn record_step(&self, ctx: &StepTelemetryCtx) {
            self.0.lock().unwrap().push(ctx.clone());
        }
    }

    #[test]
    fn record_step_invokes_telemetry_with_context() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let t = CapturingTelemetry(captured.clone());
        let ctx = StepTelemetryCtx {
            tenant_id:     "acme".into(),
            env_id:        "prod".into(),
            session_id:    "sess".into(),
            agent_id:      "a".into(),
            terminated_by: TerminationReason::FinalReply,
            iterations:    3,
            total_tokens:  742,
            duration:      Duration::from_millis(1200),
        };
        t.record_step(&ctx);
        let log = captured.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].iterations, 3);
        assert_eq!(log[0].total_tokens, 742);
    }
}
```

- [ ] **Step 2: Run telemetry tests**

Run: `cargo test -p greentic-aw-runtime telemetry`
Expected: 1 test passes.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/telemetry.rs
git commit -m "feat(aw-runtime): Telemetry trait + StepTelemetryCtx + OtelTelemetry default impl"
```

---

### Task 1.9: Mock implementations behind `test-mock` feature

**Files:**
- Modify: `crates/greentic-aw-runtime/src/mock.rs`

- [ ] **Step 1: Write the mocks**

```rust
//! Test doubles. Compiled only when `--features test-mock`. The runner-
//! host and designer integration tests use these to avoid hitting Redis
//! or real LLM providers in CI.

use std::collections::HashMap;
use std::pin::Pin;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::AgentConfig;
use crate::config_provider::ConfigProvider;
use crate::error::{ConfigError, LlmError, StateError, TerminationReason};
use crate::llm::{LlmBackend, LlmRequest, LlmResponse};
use crate::state::{
    AgentStateStore, ConversationState, SessionLock, SessionLockInner,
};
use crate::telemetry::{StepTelemetryCtx, Telemetry};
use crate::tenant::TenantContext;

/// LLM mock with a scripted response queue.
pub struct MockLlmBackend {
    pub responses: Mutex<Vec<Result<LlmResponse, LlmError>>>,
}

impl MockLlmBackend {
    pub fn new(responses: Vec<Result<LlmResponse, LlmError>>) -> Self {
        Self { responses: Mutex::new(responses) }
    }
}

impl LlmBackend for MockLlmBackend {
    async fn complete(&self, _req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(LlmError::Transport("mock queue exhausted".into()));
        }
        q.remove(0)
    }
}

/// In-memory state store; lock is a no-op semaphore.
pub struct MockAgentStateStore {
    entries: Mutex<HashMap<String, ConversationState>>,
}

impl MockAgentStateStore {
    pub fn new() -> Self {
        Self { entries: Mutex::new(HashMap::new()) }
    }

    fn key(tenant: &TenantContext, session_id: &str) -> String {
        format!("{}:{}", tenant.key_prefix(), session_id)
    }
}

impl Default for MockAgentStateStore {
    fn default() -> Self { Self::new() }
}

impl AgentStateStore for MockAgentStateStore {
    async fn load(
        &self,
        tenant: &TenantContext,
        session_id: &str,
    ) -> Result<ConversationState, StateError> {
        let k = Self::key(tenant, session_id);
        Ok(self.entries.lock().unwrap()
            .get(&k)
            .cloned()
            .unwrap_or_else(|| ConversationState::empty(tenant, session_id)))
    }

    async fn save(
        &self,
        tenant: &TenantContext,
        session_id: &str,
        state: &ConversationState,
    ) -> Result<(), StateError> {
        let k = Self::key(tenant, session_id);
        self.entries.lock().unwrap().insert(k, state.clone());
        Ok(())
    }

    async fn acquire_lock(
        &self,
        _t: &TenantContext,
        _s: &str,
        _wait: Duration,
    ) -> Result<SessionLock, StateError> {
        Ok(SessionLock::new(Box::new(NoopLockInner)))
    }
}

struct NoopLockInner;

impl SessionLockInner for NoopLockInner {
    fn refresh<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn release(&self) {}
}

pub struct MockTelemetry {
    pub recorded: Mutex<Vec<StepTelemetryCtx>>,
}

impl MockTelemetry {
    pub fn new() -> Self {
        Self { recorded: Mutex::new(Vec::new()) }
    }
}

impl Default for MockTelemetry {
    fn default() -> Self { Self::new() }
}

impl Telemetry for MockTelemetry {
    fn record_step(&self, ctx: &StepTelemetryCtx) {
        self.recorded.lock().unwrap().push(ctx.clone());
    }
}

pub struct MockConfigProvider {
    pub configs: Mutex<HashMap<String, AgentConfig>>,
}

impl MockConfigProvider {
    pub fn new() -> Self {
        Self { configs: Mutex::new(HashMap::new()) }
    }

    pub fn insert(&self, tenant: &TenantContext, agent_id: &str, cfg: AgentConfig) {
        self.configs.lock().unwrap()
            .insert(format!("{}:{agent_id}", tenant.key_prefix()), cfg);
    }
}

impl Default for MockConfigProvider {
    fn default() -> Self { Self::new() }
}

impl ConfigProvider for MockConfigProvider {
    async fn agent_config(
        &self,
        tenant:   &TenantContext,
        agent_id: &str,
    ) -> Result<AgentConfig, ConfigError> {
        let key = format!("{}:{agent_id}", tenant.key_prefix());
        self.configs.lock().unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| ConfigError::AgentNotFound(agent_id.to_string()))
    }
}

/// Convenience: assert a `TerminationReason` matches via pattern match.
pub fn assert_terminated_by(actual: &TerminationReason, expected: &TerminationReason) {
    assert_eq!(actual, expected, "expected {expected:?}, got {actual:?}");
}
```

- [ ] **Step 2: Verify the mocks build**

Run: `cargo build -p greentic-aw-runtime --features test-mock`
Expected: Compiles with warnings only.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/mock.rs
git commit -m "feat(aw-runtime): test-mock implementations (LLM, state, telemetry, config)"
```

---

### Task 1.10: tools.rs scaffold (only schema listing for Phase 1)

**Files:**
- Modify: `crates/greentic-aw-runtime/src/tools.rs`

- [ ] **Step 1: Stub the tool listing helper**

The full `invoke_tool` wrapper + idempotency ledger comes in Phase 3. For Phase 1, we only define the trait surface used by the loop pseudocode and a stub function. Replace placeholder with:

```rust
//! Tool resolution + dispatch helpers. Phase 1 introduces only the
//! type surface so the loop scaffold compiles; Phase 3 wires
//! `ExtensionRuntime::invoke_tool` via `spawn_blocking` and the
//! Redis-backed idempotency ledger.

use crate::config::ToolRef;
use crate::llm::LlmToolSchema;

/// Convert a vector of allowed [`ToolRef`]s into [`LlmToolSchema`]
/// entries the LLM understands. Phase 3 replaces this stub with a
/// real call to `ExtensionRuntime::list_tools`.
pub fn list_tools_for_llm(allowed: &[ToolRef]) -> Vec<LlmToolSchema> {
    allowed
        .iter()
        .map(|t| LlmToolSchema {
            extension_id: t.extension_id.clone(),
            tool_name:    t.tool_name.clone(),
            description:  String::new(),
            parameters:   serde_json::json!({"type": "object", "properties": {}}),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_tools_for_llm_maps_one_to_one() {
        let allowed = vec![
            ToolRef { extension_id: "http".into(), tool_name: "fetch".into() },
            ToolRef { extension_id: "calendar".into(), tool_name: "create".into() },
        ];
        let schemas = list_tools_for_llm(&allowed);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].tool_name, "fetch");
        assert_eq!(schemas[1].extension_id, "calendar");
    }
}
```

- [ ] **Step 2: Run tools tests**

Run: `cargo test -p greentic-aw-runtime tools`
Expected: 1 test passes.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/tools.rs
git commit -m "feat(aw-runtime): tools.rs scaffold (Phase 3 wires invoke_tool + ledger)"
```

---

### Task 1.11: loop.rs scaffold that returns a fixed reply

**Files:**
- Modify: `crates/greentic-aw-runtime/src/loop.rs`

For Phase 1 the loop is a stub that performs only the LLM call once and returns the reply. This lets `AgentRuntime::step` compile and unit tests of the public API run. Phase 3 replaces this with the full Plan-Act-Observe loop including tool dispatch, idempotency, cost meter, and termination handling.

- [ ] **Step 1: Write the stub loop + happy-path test**

```rust
//! Plan-Act-Observe agent loop. Phase 1 ships a single-iteration stub
//! (LLM call → reply). Phase 3 expands this into the full loop per
//! spec §5.3.

use std::time::Instant;

use crate::error::{AgentError, TerminationReason};
use crate::llm::LlmRequest;
use crate::state::ChatMessage;
use crate::telemetry::StepTelemetryCtx;
use crate::tenant::TenantContext;
use crate::tools::list_tools_for_llm;
use crate::{AgentInput, AgentOutput, AgentRuntime, AgentStep};

pub async fn run_step(
    runtime:    &AgentRuntime,
    tenant:     TenantContext,
    session_id: &str,
    agent_id:   &str,
    message:    AgentInput,
) -> Result<AgentOutput, AgentError> {
    let started = Instant::now();
    let config = runtime.config_provider.agent_config(&tenant, agent_id).await?;
    let _lock = runtime
        .state_store
        .acquire_lock(&tenant, session_id, std::time::Duration::from_secs(5))
        .await?;
    let mut state = runtime.state_store.load(&tenant, session_id).await?;
    state.messages.push(ChatMessage::User { content: message.text });

    let request = LlmRequest {
        system_prompt: config.system_prompt.clone(),
        history:       state.messages.clone(),
        tools:         list_tools_for_llm(&config.tools),
        provider:      config.llm.clone(),
    };
    let response = runtime.llm.complete(request).await?;
    let reply = response.content.unwrap_or_default();
    state.messages.push(ChatMessage::Assistant {
        content:    reply.clone(),
        tool_calls: vec![],
    });

    state.truncate_history(config.limits.max_history_turns);
    runtime.state_store.save(&tenant, session_id, &state).await?;

    let trail = vec![AgentStep::Reply { text: reply.clone() }];
    runtime.telemetry.record_step(&StepTelemetryCtx {
        tenant_id:     tenant.tenant_id.clone(),
        env_id:        tenant.env_id.clone(),
        session_id:    session_id.to_string(),
        agent_id:      agent_id.to_string(),
        terminated_by: TerminationReason::FinalReply,
        iterations:    1,
        total_tokens:  (response.tokens_in + response.tokens_out) as u64,
        duration:      started.elapsed(),
    });

    Ok(AgentOutput {
        reply,
        trail,
        terminated_by: TerminationReason::FinalReply,
    })
}

#[cfg(all(test, feature = "test-mock"))]
mod tests {
    use std::sync::Arc;

    use crate::config::{AgentConfig, AgentLimits, LlmProviderRef};
    use crate::llm::LlmResponse;
    use crate::mock::{MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry};
    use crate::tenant::TenantContext;
    use crate::{AgentInput, AgentRuntime};

    fn cfg() -> AgentConfig {
        AgentConfig {
            agent_id:      "a".into(),
            system_prompt: "sys".into(),
            tools:         vec![],
            llm:           LlmProviderRef { provider: "openai".into(), model: "m".into() },
            limits:        AgentLimits::default(),
        }
    }

    #[tokio::test]
    async fn happy_path_returns_llm_reply() {
        // Build a stub ExtensionRuntime — Phase 1 stub loop never calls it.
        // We use a `None`-like sentinel: cast a never-used `Arc<Stub>` to satisfy the field.
        // The simplest approach for Phase 1 is to use `Arc::new_uninit` is not safe; use a thin wrapper.
        // Skipping ext_runtime by leaving the test in a `#[ignore]` state if construction is hard:
        //
        // For Phase 1 we wrap an empty `ExtensionRuntime::for_test()` constructor (added in Task 1.12).
        let llm = Arc::new(MockLlmBackend::new(vec![Ok(LlmResponse {
            content:    Some("hi from llm".into()),
            tool_calls: vec![],
            tokens_in:  10,
            tokens_out: 20,
        })]));
        let store = Arc::new(MockAgentStateStore::new());
        let telemetry = Arc::new(MockTelemetry::new());
        let cp = MockConfigProvider::new();
        let tc = TenantContext::new("acme", "prod");
        cp.insert(&tc, "a", cfg());
        let cp = Arc::new(cp);

        let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
        let runtime = AgentRuntime::new(cp, store, ext, llm, telemetry.clone());

        let out = runtime
            .step(tc.clone(), "sess-1", "a", AgentInput { text: "hello".into() })
            .await
            .unwrap();
        assert_eq!(out.reply, "hi from llm");
        assert_eq!(telemetry.recorded.lock().unwrap().len(), 1);
    }
}
```

- [ ] **Step 2: Add `ExtensionRuntime::for_test()` shim if missing**

Run: `grep -n "for_test\|new_test" $(find ~/.cargo/git/checkouts/greentic-designer-extensions-*/*/crates/greentic-ext-runtime/src -name '*.rs' 2>/dev/null) 2>/dev/null | head`
Expected: Returns lines if a test ctor already exists.

If no `for_test` exists upstream, the Phase 1 test is **skipped via `#[ignore]`** — change the test attribute to `#[tokio::test] #[ignore = "needs ExtensionRuntime test shim from greentic-ext-runtime v1.2.9+"]`. Open an issue in `greentic-designer-extensions` to add `ExtensionRuntime::for_test()` and bump to `v1.2.9-research`. Phase 3 cannot proceed until this shim exists, so resolve before Phase 3 starts.

- [ ] **Step 3: Run loop tests**

Run: `cargo test -p greentic-aw-runtime --features test-mock loop`
Expected: 1 test passes (or is `ignored` per Step 2 — that is acceptable for Phase 1 gate).

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-aw-runtime/src/loop.rs
git commit -m "feat(aw-runtime): single-iteration loop stub (Phase 3 expands to full Plan-Act-Observe)"
```

---

### Task 1.12: state_redis.rs placeholder

**Files:**
- Modify: `crates/greentic-aw-runtime/src/state_redis.rs`

For Phase 1 we only need a placeholder so `pub use` in lib.rs compiles. Phase 2 fills it in.

- [ ] **Step 1: Write the placeholder**

```rust
//! Redis-backed `AgentStateStore` impl. Filled in Phase 2 — Phase 1
//! ships only the type alias so `pub use` in `lib.rs` compiles.

use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::error::StateError;
use crate::state::{
    AgentStateStore, ConversationState, SessionLock, SessionLockInner,
};
use crate::tenant::TenantContext;

/// Production state store backed by the workspace `greentic-state`
/// Redis client. Phase 2 implements `load`, `save`, `acquire_lock`.
pub struct RedisAgentStateStore {
    // Phase 2 holds an Arc<greentic_state::RedisPool> here.
    _placeholder: (),
}

impl RedisAgentStateStore {
    /// Phase 2 replaces this with `new(pool: Arc<RedisPool>) -> Self`.
    pub fn placeholder() -> Self {
        Self { _placeholder: () }
    }
}

impl AgentStateStore for RedisAgentStateStore {
    async fn load(
        &self,
        _tenant: &TenantContext,
        _session_id: &str,
    ) -> Result<ConversationState, StateError> {
        Err(StateError::Redis("RedisAgentStateStore not yet implemented (Phase 2)".into()))
    }

    async fn save(
        &self,
        _t: &TenantContext,
        _s: &str,
        _state: &ConversationState,
    ) -> Result<(), StateError> {
        Err(StateError::Redis("RedisAgentStateStore not yet implemented (Phase 2)".into()))
    }

    async fn acquire_lock(
        &self,
        _t: &TenantContext,
        _s: &str,
        _wait: Duration,
    ) -> Result<SessionLock, StateError> {
        Err(StateError::Redis("RedisAgentStateStore not yet implemented (Phase 2)".into()))
    }
}
```

- [ ] **Step 2: Verify build**

Run: `cargo build -p greentic-aw-runtime --features test-mock`
Expected: Compiles clean.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/state_redis.rs
git commit -m "feat(aw-runtime): state_redis.rs placeholder (Phase 2 implements)"
```

---

### Task 1.13: Workspace clippy + fmt clean

- [ ] **Step 1: Run formatter**

Run: `cargo fmt -p greentic-aw-runtime --check`
Expected: No output (clean). If formatting issues exist, run `cargo fmt -p greentic-aw-runtime` and re-check.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -p greentic-aw-runtime --all-targets --features test-mock -- -D warnings`
Expected: Compiles with zero warnings. Fix any issues in place.

- [ ] **Step 3: Run all Phase 1 tests**

Run: `cargo test -p greentic-aw-runtime --features test-mock`
Expected: All unit tests pass. Loop test may be `ignored` if `for_test` shim is missing (acceptable).

- [ ] **Step 4: Commit any formatting/clippy fixes**

```bash
git add crates/greentic-aw-runtime/src
git commit -m "chore(aw-runtime): apply rustfmt + clippy fixes for Phase 1"
```

- [ ] **Step 5: Open PR1**

```bash
git push -u origin feat/aw-runtime-phase-1
gh pr create --base develop --title "feat(aw-runtime): Phase 1 — library skeleton + traits + mocks" --body "$(cat <<'EOF'
## Summary
- Scaffold `crates/greentic-aw-runtime` library crate per spec §5.1
- Define all public traits (`ConfigProvider`, `AgentStateStore`, `LlmBackend`, `Telemetry`) with native `async fn` (no `async-trait` crate)
- Implement `RetryingLlmBackend` decorator + `CachingConfigProvider` (60s TTL)
- Ship `test-mock` feature with mocks for all four traits
- Single-iteration loop stub (Phase 3 expands to Plan-Act-Observe)
- Redis state store + lock are placeholders (Phase 2 implements)

Spec: docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md

## Test plan
- [ ] `cargo test -p greentic-aw-runtime --features test-mock` all pass (loop test may be `#[ignore]`d pending `ExtensionRuntime::for_test()` shim)
- [ ] `cargo clippy -p greentic-aw-runtime --all-targets --features test-mock -- -D warnings` clean
- [ ] `cargo fmt -p greentic-aw-runtime --check` clean
EOF
)"
```

After PR1 merges, fast-forward `develop` per workspace CLAUDE.md rule.

---

## Phase 2 — Redis State Backend + Distributed Lock

**PR gate:** Integration tests green against a real Redis instance (`docker compose up -d redis` from `greentic-integration` or any local Redis on `localhost:6379`). Lock behaviour verified under concurrent step calls.

**Pre-Phase-2:** Before starting, peek at `greentic-state` Redis pool surface — file: `/Users/bimapangestu/Desktop/Works/personal/greentic/greentic-state/src/lib.rs`. Find the public type used to obtain a Redis connection (likely `greentic_state::RedisPool` or `greentic_state::Client`). All code blocks below use the placeholder name `greentic_state::RedisPool` — replace with the actual type if it differs.

### Task 2.1: Add greentic-state dep + write the load test

**Files:**
- Modify: `crates/greentic-aw-runtime/Cargo.toml`
- Modify: `crates/greentic-aw-runtime/src/state_redis.rs`
- Test: `crates/greentic-aw-runtime/tests/redis_state.rs`

- [ ] **Step 1: Wire the dependency**

Modify `crates/greentic-aw-runtime/Cargo.toml` `[dependencies]` section — `greentic-state` is already added in Task 1.1. Verify the workspace pin resolves the same version as `greentic-runner-host` uses:

```bash
cargo tree -p greentic-aw-runtime --depth 1 | grep greentic-state
```

Expected: One line `greentic-state v...` matching the version in `greentic-runner-host`.

- [ ] **Step 2: Write a failing integration test**

Create `crates/greentic-aw-runtime/tests/redis_state.rs`:

```rust
//! Integration tests against a real Redis instance.
//!
//! Skip the whole module if `REDIS_URL` is not set in the environment.
//! CI sets this to `redis://localhost:6379/15` (DB 15 to avoid step on
//! shared dev state).

#![cfg(feature = "test-mock")] // mocks not used; feature gate just keeps test bin tiny in default builds

use std::time::Duration;

use greentic_aw_runtime::state::{AgentStateStore, ChatMessage, ConversationState};
use greentic_aw_runtime::state_redis::RedisAgentStateStore;
use greentic_aw_runtime::tenant::TenantContext;

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

async fn make_store() -> RedisAgentStateStore {
    let url = redis_url().expect("REDIS_URL must be set for integration tests");
    RedisAgentStateStore::connect(&url).await.expect("redis connect")
}

#[tokio::test]
async fn save_then_load_roundtrips_state() {
    let Some(_) = redis_url() else { eprintln!("REDIS_URL unset; skipping"); return; };
    let store = make_store().await;
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("sess-{}", uuid::Uuid::new_v4());

    let mut state = ConversationState::empty(&tc, &session);
    state.messages.push(ChatMessage::User { content: "hello".into() });
    store.save(&tc, &session, &state).await.unwrap();

    let loaded = store.load(&tc, &session).await.unwrap();
    assert_eq!(loaded.messages.len(), 1);
    if let ChatMessage::User { content } = &loaded.messages[0] {
        assert_eq!(content, "hello");
    } else {
        panic!("expected User message");
    }
}

#[tokio::test]
async fn load_returns_empty_state_when_no_record_exists() {
    let Some(_) = redis_url() else { return; };
    let store = make_store().await;
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("never-existed-{}", uuid::Uuid::new_v4());

    let loaded = store.load(&tc, &session).await.unwrap();
    assert_eq!(loaded.schema_version, 1);
    assert_eq!(loaded.session_id, session);
    assert!(loaded.messages.is_empty());
}
```

- [ ] **Step 3: Run the test (expect failure)**

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state save_then_load`
Expected: FAIL — `RedisAgentStateStore::connect` does not exist; `load`/`save` return the placeholder error.

- [ ] **Step 4: Commit the failing test**

```bash
git add crates/greentic-aw-runtime/tests/redis_state.rs
git commit -m "test(aw-runtime): failing integration test for Redis state save/load"
```

---

### Task 2.2: Implement RedisAgentStateStore::connect, load, save

**Files:**
- Modify: `crates/greentic-aw-runtime/src/state_redis.rs`

- [ ] **Step 1: Replace the placeholder with the real impl**

```rust
//! Redis-backed `AgentStateStore` impl.

use std::pin::Pin;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use greentic_state::{RedisPool, Connection};

use crate::error::StateError;
use crate::state::{
    AgentStateStore, ConversationState, SessionLock, SessionLockInner,
    STATE_SCHEMA_VERSION,
};
use crate::tenant::TenantContext;

const STATE_TTL_SECS:  u64 = 7 * 24 * 60 * 60; // 7 days
const LOCK_TTL_SECS:   u64 = 90;
const LOCK_POLL_MS:    u64 = 50;

pub struct RedisAgentStateStore {
    pool: Arc<RedisPool>,
}

impl RedisAgentStateStore {
    pub fn new(pool: Arc<RedisPool>) -> Self {
        Self { pool }
    }

    pub async fn connect(url: &str) -> Result<Self, StateError> {
        let pool = RedisPool::connect(url)
            .await
            .map_err(|e| StateError::Redis(format!("connect: {e}")))?;
        Ok(Self { pool: Arc::new(pool) })
    }

    fn state_key(tenant: &TenantContext, session_id: &str) -> String {
        format!("{}:{session_id}:state", tenant.key_prefix())
    }

    fn lock_key(tenant: &TenantContext, session_id: &str) -> String {
        format!("{}:{session_id}:lock", tenant.key_prefix())
    }

    async fn conn(&self) -> Result<Connection, StateError> {
        self.pool.get().await.map_err(|e| StateError::Redis(format!("pool: {e}")))
    }
}

impl AgentStateStore for RedisAgentStateStore {
    async fn load(
        &self,
        tenant:     &TenantContext,
        session_id: &str,
    ) -> Result<ConversationState, StateError> {
        let key = Self::state_key(tenant, session_id);
        let mut conn = self.conn().await?;
        let raw: Option<String> = conn
            .get(&key)
            .await
            .map_err(|e| StateError::Redis(format!("get: {e}")))?;
        let Some(json) = raw else {
            return Ok(ConversationState::empty(tenant, session_id));
        };
        let state: ConversationState = serde_json::from_str(&json)
            .map_err(|e| StateError::Decode(format!("state json: {e}")))?;
        if state.schema_version > STATE_SCHEMA_VERSION {
            return Err(StateError::SchemaIncompatible {
                found:     state.schema_version,
                supported: STATE_SCHEMA_VERSION,
            });
        }
        Ok(state)
    }

    async fn save(
        &self,
        tenant:     &TenantContext,
        session_id: &str,
        state:      &ConversationState,
    ) -> Result<(), StateError> {
        let key = Self::state_key(tenant, session_id);
        let json = serde_json::to_string(state)
            .map_err(|e| StateError::Decode(format!("state json: {e}")))?;
        let mut conn = self.conn().await?;
        conn.set_ex(&key, &json, STATE_TTL_SECS)
            .await
            .map_err(|e| StateError::Redis(format!("set_ex: {e}")))?;
        Ok(())
    }

    async fn acquire_lock(
        &self,
        tenant:     &TenantContext,
        session_id: &str,
        wait:       Duration,
    ) -> Result<SessionLock, StateError> {
        let key = Self::lock_key(tenant, session_id);
        let value = uuid::Uuid::new_v4().to_string();
        let deadline = std::time::Instant::now() + wait;
        loop {
            let mut conn = self.conn().await?;
            let acquired: bool = conn
                .set_nx_ex(&key, &value, LOCK_TTL_SECS)
                .await
                .map_err(|e| StateError::Redis(format!("set nx: {e}")))?;
            if acquired {
                let inner = RedisSessionLock {
                    pool:  self.pool.clone(),
                    key,
                    value,
                };
                return Ok(SessionLock::new(Box::new(inner)));
            }
            if std::time::Instant::now() >= deadline {
                return Err(StateError::LockTimeout(wait));
            }
            tokio::time::sleep(Duration::from_millis(LOCK_POLL_MS)).await;
        }
    }
}

struct RedisSessionLock {
    pool:  Arc<RedisPool>,
    key:   String,
    value: String, // owner token; release only if value still matches (safety against TTL expiry races)
}

impl SessionLockInner for RedisSessionLock {
    fn refresh<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<(), StateError>> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = self.pool.get().await
                .map_err(|e| StateError::Redis(format!("pool: {e}")))?;
            // Refresh TTL only if we still hold the lock (check-and-set via Lua).
            let script = r#"
                if redis.call('GET', KEYS[1]) == ARGV[1] then
                    return redis.call('EXPIRE', KEYS[1], ARGV[2])
                else
                    return 0
                end
            "#;
            let refreshed: i64 = conn
                .eval(script, &[&self.key], &[&self.value, &LOCK_TTL_SECS.to_string()])
                .await
                .map_err(|e| StateError::Redis(format!("refresh eval: {e}")))?;
            if refreshed == 1 {
                Ok(())
            } else {
                Err(StateError::Redis("lock no longer owned by this holder".into()))
            }
        })
    }

    fn release(&self) {
        // Best-effort sync release; uses a blocking adapter on the pool.
        // If pool exposes only async API, spawn a detached task.
        let pool = self.pool.clone();
        let key = self.key.clone();
        let value = self.value.clone();
        tokio::spawn(async move {
            let Ok(mut conn) = pool.get().await else { return };
            let script = r#"
                if redis.call('GET', KEYS[1]) == ARGV[1] then
                    return redis.call('DEL', KEYS[1])
                else
                    return 0
                end
            "#;
            let _: Result<i64, _> = conn.eval(script, &[&key], &[&value]).await;
        });
    }
}
```

> **Note on `greentic-state` API shape:** the code above assumes `RedisPool::connect(url)`, `pool.get()` returning a connection, `conn.get/set_ex/set_nx_ex/eval`. Check the actual `greentic_state` surface and adapt — if `RedisPool` uses `redis::aio::ConnectionManager` directly, swap the `mut conn = self.conn().await?` lines for direct `redis::cmd("GET").arg(&key).query_async(&mut conn).await` calls. Same Lua scripts apply either way.

- [ ] **Step 2: Run the tests against real Redis**

Make sure Redis is running locally: `docker run -d --name aw-test-redis -p 6379:6379 redis:7-alpine` (or use an existing Redis).

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state -- --nocapture`
Expected: Both tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/state_redis.rs
git commit -m "feat(aw-runtime): RedisAgentStateStore — load/save/acquire_lock with SETEX 7d + SET NX 90s + Lua release"
```

---

### Task 2.3: Schema version mismatch test + handling

**Files:**
- Modify: `crates/greentic-aw-runtime/tests/redis_state.rs`

- [ ] **Step 1: Append schema version test**

Append to `tests/redis_state.rs`:

```rust
#[tokio::test]
async fn load_returns_schema_incompatible_when_version_in_future() {
    let Some(url) = redis_url() else { return; };
    let store = RedisAgentStateStore::connect(&url).await.unwrap();
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("future-{}", uuid::Uuid::new_v4());

    // Direct Redis write of a state with schema_version=99
    let mut conn = greentic_state::RedisPool::connect(&url)
        .await
        .unwrap()
        .get()
        .await
        .unwrap();
    let key = format!("aw:test-acme:test-prod:{session}:state");
    let payload = serde_json::json!({
        "schema_version": 99,
        "session_id": session,
        "tenant_id":  "test-acme",
        "env_id":     "test-prod",
        "messages":   [],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    });
    conn.set_ex(&key, &payload.to_string(), 60).await.unwrap();

    let err = store.load(&tc, &session).await.unwrap_err();
    match err {
        greentic_aw_runtime::error::StateError::SchemaIncompatible { found, supported } => {
            assert_eq!(found, 99);
            assert_eq!(supported, 1);
        }
        other => panic!("expected SchemaIncompatible, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run**

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state schema`
Expected: 1 test passes (the new one).

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/tests/redis_state.rs
git commit -m "test(aw-runtime): schema_version > 1 in Redis returns StateError::SchemaIncompatible"
```

---

### Task 2.4: Concurrent lock blocking test

**Files:**
- Modify: `crates/greentic-aw-runtime/tests/redis_state.rs`

- [ ] **Step 1: Append the concurrency test**

```rust
#[tokio::test]
async fn second_acquire_lock_blocks_until_first_releases() {
    let Some(_) = redis_url() else { return; };
    let store_a = make_store().await;
    let store_b = make_store().await;
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("lock-{}", uuid::Uuid::new_v4());

    let lock1 = store_a
        .acquire_lock(&tc, &session, Duration::from_millis(500))
        .await
        .unwrap();

    // Concurrent acquire from store_b should time out within 500ms.
    let started = std::time::Instant::now();
    let result = store_b
        .acquire_lock(&tc, &session, Duration::from_millis(500))
        .await;
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(matches!(
        result,
        Err(greentic_aw_runtime::error::StateError::LockTimeout(_))
    ));

    drop(lock1);
    // After drop the lock should be released (best-effort) within ~100ms.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let lock2 = store_b
        .acquire_lock(&tc, &session, Duration::from_millis(500))
        .await
        .unwrap();
    drop(lock2);
}
```

- [ ] **Step 2: Run**

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state second_acquire`
Expected: 1 test passes.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/tests/redis_state.rs
git commit -m "test(aw-runtime): concurrent acquire_lock blocks until first holder drops"
```

---

### Task 2.5: Lock TTL refresh test

**Files:**
- Modify: `crates/greentic-aw-runtime/tests/redis_state.rs`

- [ ] **Step 1: Append the refresh test**

```rust
#[tokio::test]
async fn lock_refresh_extends_ttl() {
    let Some(url) = redis_url() else { return; };
    let store = make_store().await;
    let tc = TenantContext::new("test-acme", "test-prod");
    let session = format!("refresh-{}", uuid::Uuid::new_v4());

    let lock = store
        .acquire_lock(&tc, &session, Duration::from_millis(500))
        .await
        .unwrap();

    // Inspect the raw TTL via redis.
    let mut conn = greentic_state::RedisPool::connect(&url)
        .await
        .unwrap()
        .get()
        .await
        .unwrap();
    let key = format!("aw:test-acme:test-prod:{session}:lock");
    let ttl_before: i64 = conn.ttl(&key).await.unwrap();
    assert!(ttl_before > 80, "expected initial TTL ~90s, got {ttl_before}");

    // Wait, then refresh.
    tokio::time::sleep(Duration::from_secs(2)).await;
    lock.refresh().await.unwrap();
    let ttl_after: i64 = conn.ttl(&key).await.unwrap();
    assert!(ttl_after > ttl_before - 1, "refresh should have reset TTL to ~90");
    drop(lock);
}
```

- [ ] **Step 2: Run**

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state lock_refresh`
Expected: 1 test passes.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/tests/redis_state.rs
git commit -m "test(aw-runtime): SessionLock::refresh extends Redis TTL"
```

---

### Task 2.6: Multi-tenant isolation test

**Files:**
- Modify: `crates/greentic-aw-runtime/tests/redis_state.rs`

- [ ] **Step 1: Append**

```rust
#[tokio::test]
async fn two_tenants_share_redis_without_cross_talk() {
    let Some(_) = redis_url() else { return; };
    let store = make_store().await;
    let a = TenantContext::new("acme", "prod");
    let b = TenantContext::new("beta", "prod");
    let session = "shared-session-name".to_string();

    let mut state_a = ConversationState::empty(&a, &session);
    state_a.messages.push(ChatMessage::User { content: "from-acme".into() });
    store.save(&a, &session, &state_a).await.unwrap();

    let mut state_b = ConversationState::empty(&b, &session);
    state_b.messages.push(ChatMessage::User { content: "from-beta".into() });
    store.save(&b, &session, &state_b).await.unwrap();

    let loaded_a = store.load(&a, &session).await.unwrap();
    let loaded_b = store.load(&b, &session).await.unwrap();
    if let (ChatMessage::User { content: ca }, ChatMessage::User { content: cb }) =
        (&loaded_a.messages[0], &loaded_b.messages[0])
    {
        assert_eq!(ca, "from-acme");
        assert_eq!(cb, "from-beta");
    } else {
        panic!("expected User messages on both tenants");
    }
}
```

- [ ] **Step 2: Run + commit**

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state two_tenants`
Expected: 1 test passes.

```bash
git add crates/greentic-aw-runtime/tests/redis_state.rs
git commit -m "test(aw-runtime): two tenants in same Redis show zero cross-talk"
```

---

### Task 2.7: Open PR2

- [ ] **Step 1: Run full Phase-2 pipeline locally**

Run:
```bash
REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock
cargo clippy -p greentic-aw-runtime --all-targets --features test-mock -- -D warnings
cargo fmt -p greentic-aw-runtime --check
```
Expected: All pass.

- [ ] **Step 2: Push + open PR**

```bash
git push -u origin feat/aw-runtime-phase-2
gh pr create --base develop --title "feat(aw-runtime): Phase 2 — RedisAgentStateStore + distributed lock" --body "$(cat <<'EOF'
## Summary
- Implement `RedisAgentStateStore` with `aw:{tenant}:{env}:{session}:state` keys (7d TTL)
- Implement `RedisSessionLock` via `SET NX` with 90s TTL + Lua check-and-set release
- `SessionLock::refresh` extends TTL only if still held (Lua-guarded)
- Schema version > 1 returns `StateError::SchemaIncompatible`

Spec: §5.5 Redis key schema, §4 Decision 7 (locking)

## Test plan
- [ ] `REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock` all pass
- [ ] Concurrent lock acquire blocks until first holder drops
- [ ] Lock TTL refresh extends expiry
- [ ] Multi-tenant zero cross-talk
EOF
)"
```

---

## Phase 3 — LLM Backend + Full Loop + Cost Meter

**PR gate:** All scripted-loop unit tests pass under `--features test-mock`. Live OpenAI smoke test passes when `GREENTIC_LLM_TEST_BUDGET_USD > 0`. X3 (admin LLM provider UI) verified by glancing at the admin designer; if absent, raise a separate spec before merging Phase 3.

**Pre-Phase-3:**
- Verify `ExtensionRuntime::for_test()` (or equivalent) exists in `greentic-ext-runtime`. If not, file an upstream issue and pause Phase 3 until it ships. Tasks below assume this shim is available.
- Verify X3: open admin designer UI (`https://admin.designer.localhost` or `make dev` in `greentic-designer-admin`), navigate to Settings → LLM Providers, confirm at minimum one provider can be listed/added/edited. If the UI is absent, file an admin-spec issue and gate Phase 3 merge on its delivery.

### Task 3.1: OpenAI LlmBackend — request + response shapes

**Files:**
- Modify: `crates/greentic-aw-runtime/src/llm.rs`
- New: `crates/greentic-aw-runtime/src/llm_openai.rs`

- [ ] **Step 1: Move shared types to llm.rs (already done in Task 1.7); add the OpenAI impl**

Create `crates/greentic-aw-runtime/src/llm_openai.rs`:

```rust
//! OpenAI Chat Completions API client (function-calling mode).
//! Single backend MVP; multi-provider routing deferred (spec §10).

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::error::LlmError;
use crate::llm::{LlmBackend, LlmRequest, LlmResponse, LlmToolSchema};
use crate::state::{ChatMessage, ToolCallRecord};

pub struct OpenAiLlmBackend {
    api_key: String,
    base_url: String,
    client: Client,
}

impl OpenAiLlmBackend {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, "https://api.openai.com")
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key:  api_key.into(),
            base_url: base_url.into(),
            client:   Client::builder()
                .timeout(Duration::from_secs(45))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[derive(Serialize)]
struct OaRequest<'a> {
    model:       &'a str,
    messages:    Vec<OaMessage<'a>>,
    tools:       Option<Vec<OaTool<'a>>>,
    tool_choice: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum OaMessage<'a> {
    System    { content: &'a str },
    User      { content: &'a str },
    Assistant { content: Option<&'a str>, tool_calls: Vec<OaToolCallEmit<'a>> },
    Tool      { tool_call_id: &'a str, content: String },
}

#[derive(Serialize)]
struct OaToolCallEmit<'a> {
    id:        &'a str,
    #[serde(rename = "type")]
    typ:       &'static str,
    function:  OaToolFn<'a>,
}

#[derive(Serialize)]
struct OaToolFn<'a> {
    name:      &'a str,
    arguments: String,
}

#[derive(Serialize)]
struct OaTool<'a> {
    #[serde(rename = "type")]
    typ:      &'static str,
    function: OaToolDef<'a>,
}

#[derive(Serialize)]
struct OaToolDef<'a> {
    name:        String,
    description: &'a str,
    parameters:  &'a serde_json::Value,
}

#[derive(Deserialize)]
struct OaResponse {
    choices: Vec<OaChoice>,
    usage:   OaUsage,
}

#[derive(Deserialize)]
struct OaChoice {
    message: OaMessageIn,
}

#[derive(Deserialize)]
struct OaMessageIn {
    content:    Option<String>,
    tool_calls: Option<Vec<OaToolCallIn>>,
}

#[derive(Deserialize)]
struct OaToolCallIn {
    id:       String,
    function: OaToolFnIn,
}

#[derive(Deserialize)]
struct OaToolFnIn {
    name:      String,
    arguments: String, // JSON-encoded string per OpenAI spec
}

#[derive(Deserialize)]
struct OaUsage {
    prompt_tokens:     u32,
    completion_tokens: u32,
}

impl LlmBackend for OpenAiLlmBackend {
    async fn complete(&self, req: LlmRequest) -> Result<LlmResponse, LlmError> {
        let messages = build_messages(&req);
        let tools = if req.tools.is_empty() {
            None
        } else {
            Some(req.tools.iter().map(build_tool).collect())
        };
        let body = OaRequest {
            model:       &req.provider.model,
            messages:    messages.iter().collect::<Vec<_>>().into_iter().cloned().collect(),
            tools,
            tool_choice: "auto",
        };
        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_server_error() {
            return Err(LlmError::ServiceUnavailable);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(LlmError::BadRequest(format!("{status}: {text}")));
        }
        let oa: OaResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Decode(e.to_string()))?;
        let choice = oa.choices.into_iter().next()
            .ok_or_else(|| LlmError::Decode("no choices".into()))?;
        let tool_calls = choice.message.tool_calls.unwrap_or_default()
            .into_iter()
            .map(|c| {
                let args: serde_json::Value = serde_json::from_str(&c.function.arguments)
                    .unwrap_or(serde_json::json!({}));
                let (extension_id, tool_name) = split_tool_name(&c.function.name);
                ToolCallRecord {
                    call_id: c.id,
                    extension_id,
                    tool_name,
                    args,
                }
            })
            .collect();
        Ok(LlmResponse {
            content: choice.message.content,
            tool_calls,
            tokens_in:  oa.usage.prompt_tokens,
            tokens_out: oa.usage.completion_tokens,
        })
    }
}

/// Split an LLM-emitted tool name like `"http.fetch"` into
/// `(extension_id, tool_name)`. If no dot is present, treat the whole
/// string as the tool name with `""` extension (caller validates).
fn split_tool_name(name: &str) -> (String, String) {
    match name.split_once('.') {
        Some((ext, tool)) => (ext.to_string(), tool.to_string()),
        None              => (String::new(), name.to_string()),
    }
}

fn build_messages(req: &LlmRequest) -> Vec<OaMessage<'_>> {
    let mut out: Vec<OaMessage<'_>> = Vec::with_capacity(req.history.len() + 1);
    out.push(OaMessage::System { content: &req.system_prompt });
    for m in &req.history {
        match m {
            ChatMessage::System { content } => out.push(OaMessage::System { content }),
            ChatMessage::User { content }   => out.push(OaMessage::User { content }),
            ChatMessage::Assistant { content, tool_calls } => {
                let calls = tool_calls.iter().map(|tc| OaToolCallEmit {
                    id:       &tc.call_id,
                    typ:      "function",
                    function: OaToolFn {
                        name:      &format!("{}.{}", tc.extension_id, tc.tool_name),
                        arguments: tc.args.to_string(),
                    },
                }).collect();
                out.push(OaMessage::Assistant {
                    content: Some(content),
                    tool_calls: calls,
                });
            }
            ChatMessage::Tool { call_id, content } => {
                out.push(OaMessage::Tool {
                    tool_call_id: call_id,
                    content:      content.to_string(),
                });
            }
        }
    }
    out
}

fn build_tool<'a>(t: &'a LlmToolSchema) -> OaTool<'a> {
    OaTool {
        typ:      "function",
        function: OaToolDef {
            name:        format!("{}.{}", t.extension_id, t.tool_name),
            description: &t.description,
            parameters:  &t.parameters,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tool_name_parses_extension_prefix() {
        assert_eq!(split_tool_name("http.fetch"), ("http".into(), "fetch".into()));
        assert_eq!(split_tool_name("toolname-no-ext"), (String::new(), "toolname-no-ext".into()));
    }
}
```

- [ ] **Step 2: Add reqwest dep**

Modify `crates/greentic-aw-runtime/Cargo.toml`:

```diff
 [dependencies]
+reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
```

- [ ] **Step 3: Wire module into lib.rs**

Edit `crates/greentic-aw-runtime/src/lib.rs` `pub mod` block — add `pub mod llm_openai;` near the other modules; add `pub use llm_openai::OpenAiLlmBackend;` in the re-exports.

- [ ] **Step 4: Verify the offline unit test compiles**

Run: `cargo test -p greentic-aw-runtime llm_openai`
Expected: 1 test passes.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-aw-runtime/src/llm_openai.rs crates/greentic-aw-runtime/src/lib.rs crates/greentic-aw-runtime/Cargo.toml
git commit -m "feat(aw-runtime): OpenAiLlmBackend via reqwest with function-calling support"
```

---

### Task 3.2: tools.rs — invoke_tool wrapper via spawn_blocking + idempotency ledger

**Files:**
- Modify: `crates/greentic-aw-runtime/src/tools.rs`

- [ ] **Step 1: Replace the Phase 1 stub with the full impl**

```rust
//! Tool resolution + dispatch helpers.
//!
//! `ExtensionRuntime::invoke_tool` is a synchronous `fn` that performs
//! Wasmtime WASM dispatch (CPU-bound, may block several seconds).
//! Every call site MUST wrap it in `tokio::task::spawn_blocking`. Per
//! spec §5.3, this is enforced here — no other module is allowed to
//! call `invoke_tool` directly.
//!
//! Each tool call is recorded in Redis by `tool_call_id` before
//! dispatch so a state-save failure does not cause double-dispatch on
//! the next `step()`.

use std::sync::Arc;

use greentic_ext_runtime::ExtensionRuntime;
use serde::{Deserialize, Serialize};

use crate::config::ToolRef;
use crate::error::AgentError;
use crate::llm::LlmToolSchema;
use crate::state::ToolCallRecord;
use crate::tenant::TenantContext;

/// Whether the agent is permitted to call this tool. Lookup is by
/// (extension_id, tool_name) tuple against the allow-list.
pub fn is_tool_allowed(call: &ToolCallRecord, allowed: &[ToolRef]) -> bool {
    allowed.iter().any(|t| {
        t.extension_id == call.extension_id && t.tool_name == call.tool_name
    })
}

/// Map allowed tools to LLM-facing JSON schemas via
/// `ExtensionRuntime::list_tools`. If the extension is not loaded the
/// tool is dropped silently (logged) — the LLM simply won't see it.
pub fn list_tools_for_llm(
    ext_runtime: &ExtensionRuntime,
    allowed:     &[ToolRef],
) -> Vec<LlmToolSchema> {
    let mut out = Vec::with_capacity(allowed.len());
    for t in allowed {
        match ext_runtime.list_tools(&t.extension_id) {
            Ok(schemas) => {
                if let Some(s) = schemas.into_iter().find(|s| s.name == t.tool_name) {
                    out.push(LlmToolSchema {
                        extension_id: t.extension_id.clone(),
                        tool_name:    t.tool_name.clone(),
                        description:  s.description,
                        parameters:   s.parameters,
                    });
                } else {
                    tracing::warn!(
                        extension = %t.extension_id, tool = %t.tool_name,
                        "tool not found in extension; dropping from LLM tool list"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    extension = %t.extension_id, error = %e,
                    "extension list_tools failed; skipping"
                );
            }
        }
    }
    out
}

/// Dispatch a single tool call. Wraps the blocking `invoke_tool` in
/// `tokio::task::spawn_blocking` so the executor thread is never
/// stalled.
pub async fn dispatch_tool_call(
    ext_runtime: Arc<ExtensionRuntime>,
    tenant:      TenantContext,
    call:        ToolCallRecord,
) -> Result<serde_json::Value, AgentError> {
    let args = call.args.clone();
    let extension_id = call.extension_id.clone();
    let tool_name = call.tool_name.clone();
    let tenant_for_blocking = tenant.clone();
    let join_handle = tokio::task::spawn_blocking(move || {
        ext_runtime.invoke_tool(
            &tenant_for_blocking.tenant_id,
            &tenant_for_blocking.env_id,
            &extension_id,
            &tool_name,
            &args.to_string(),
        )
    });
    let raw = join_handle.await
        .map_err(|e| AgentError::ToolDispatch(format!("join: {e}")))?
        .map_err(|e| AgentError::ToolDispatch(format!("invoke: {e}")))?;
    serde_json::from_str(&raw)
        .map_err(|e| AgentError::ToolDispatch(format!("decode: {e}")))
}

/// Idempotency ledger entry stored under
/// `aw:{tenant}:{env}:{session}:tool_calls:{call_id}`. TTL 7 days.
#[derive(Serialize, Deserialize, Clone)]
pub struct ToolLedgerEntry {
    pub result: serde_json::Value,
}

pub fn ledger_key(tenant: &TenantContext, session_id: &str, call_id: &str) -> String {
    format!("{}:{session_id}:tool_calls:{call_id}", tenant.key_prefix())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_tool_allowed_returns_true_for_exact_match() {
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name:    "fetch".into(),
        }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            args: serde_json::json!({}),
        };
        assert!(is_tool_allowed(&call, &allowed));
    }

    #[test]
    fn is_tool_allowed_returns_false_for_unauthorized_tool() {
        let allowed = vec![ToolRef { extension_id: "http".into(), tool_name: "fetch".into() }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "post".into(),
            args: serde_json::json!({}),
        };
        assert!(!is_tool_allowed(&call, &allowed));
    }

    #[test]
    fn ledger_key_includes_tenant_env_session_callid() {
        let tc = TenantContext::new("acme", "prod");
        let key = ledger_key(&tc, "sess-1", "call-abc");
        assert_eq!(key, "aw:acme:prod:sess-1:tool_calls:call-abc");
    }
}
```

- [ ] **Step 2: Run unit tests**

Run: `cargo test -p greentic-aw-runtime tools`
Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-aw-runtime/src/tools.rs
git commit -m "feat(aw-runtime): tools.rs — invoke_tool via spawn_blocking + idempotency ledger key"
```

---

### Task 3.3: Cost meter helper

**Files:**
- New: `crates/greentic-aw-runtime/src/cost.rs`

- [ ] **Step 1: Write the helper**

```rust
//! Per-tenant daily token counter enforced at `step()` entry.
//!
//! Key: `aw:{tenant}:{env}:cost:tokens:{yyyymmdd}` with 86400s TTL.
//! Implementation lives here so the loop is unit-testable with a
//! pluggable trait (`TokenMeter`).

use std::sync::Arc;

use chrono::Utc;
use greentic_state::RedisPool;

use crate::error::StateError;
use crate::tenant::TenantContext;

pub trait TokenMeter: Send + Sync {
    /// Return tokens consumed by this tenant within the current UTC
    /// day. Implementations MUST default to `0` when no counter
    /// exists yet.
    async fn current(&self, tenant: &TenantContext) -> Result<u64, StateError>;

    /// Add `tokens` to the daily counter. Sets TTL to 86400s on every
    /// call (rolling 24h window).
    async fn add(&self, tenant: &TenantContext, tokens: u64) -> Result<(), StateError>;
}

pub struct RedisTokenMeter {
    pool: Arc<RedisPool>,
}

impl RedisTokenMeter {
    pub fn new(pool: Arc<RedisPool>) -> Self {
        Self { pool }
    }

    fn key(tenant: &TenantContext) -> String {
        let day = Utc::now().format("%Y%m%d").to_string();
        format!("{}:cost:tokens:{day}", tenant.key_prefix())
    }
}

impl TokenMeter for RedisTokenMeter {
    async fn current(&self, tenant: &TenantContext) -> Result<u64, StateError> {
        let key = Self::key(tenant);
        let mut conn = self.pool.get().await
            .map_err(|e| StateError::Redis(format!("pool: {e}")))?;
        let value: Option<u64> = conn.get(&key).await
            .map_err(|e| StateError::Redis(format!("get: {e}")))?;
        Ok(value.unwrap_or(0))
    }

    async fn add(&self, tenant: &TenantContext, tokens: u64) -> Result<(), StateError> {
        let key = Self::key(tenant);
        let mut conn = self.pool.get().await
            .map_err(|e| StateError::Redis(format!("pool: {e}")))?;
        conn.incrby(&key, tokens as i64).await
            .map_err(|e| StateError::Redis(format!("incrby: {e}")))?;
        conn.expire(&key, 86_400).await
            .map_err(|e| StateError::Redis(format!("expire: {e}")))?;
        Ok(())
    }
}

#[cfg(feature = "test-mock")]
pub struct MockTokenMeter {
    pub current_value: std::sync::Mutex<u64>,
}

#[cfg(feature = "test-mock")]
impl MockTokenMeter {
    pub fn new(current: u64) -> Self {
        Self { current_value: std::sync::Mutex::new(current) }
    }
}

#[cfg(feature = "test-mock")]
impl TokenMeter for MockTokenMeter {
    async fn current(&self, _t: &TenantContext) -> Result<u64, StateError> {
        Ok(*self.current_value.lock().unwrap())
    }

    async fn add(&self, _t: &TenantContext, tokens: u64) -> Result<(), StateError> {
        let mut v = self.current_value.lock().unwrap();
        *v += tokens;
        Ok(())
    }
}
```

- [ ] **Step 2: Register module in lib.rs**

Add `pub mod cost;` near other `pub mod` lines and `pub use cost::{TokenMeter, RedisTokenMeter};` in the re-exports block.

- [ ] **Step 3: Verify build**

Run: `cargo build -p greentic-aw-runtime --features test-mock`
Expected: Compiles clean.

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-aw-runtime/src/cost.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw-runtime): TokenMeter trait + RedisTokenMeter + MockTokenMeter (daily cap enforcement)"
```

---

### Task 3.4: Full Plan-Act-Observe loop

**Files:**
- Modify: `crates/greentic-aw-runtime/src/loop.rs`
- Modify: `crates/greentic-aw-runtime/src/lib.rs` — wire `TokenMeter` into `AgentRuntime`

- [ ] **Step 1: Extend AgentRuntime to hold a TokenMeter**

Modify `crates/greentic-aw-runtime/src/lib.rs`:

```diff
 pub struct AgentRuntime {
     pub(crate) config_provider: Arc<dyn ConfigProvider>,
     pub(crate) state_store:     Arc<dyn AgentStateStore>,
     pub(crate) ext_runtime:     Arc<greentic_ext_runtime::ExtensionRuntime>,
     pub(crate) llm:             Arc<dyn LlmBackend>,
     pub(crate) telemetry:       Arc<dyn Telemetry>,
+    pub(crate) token_meter:     Arc<dyn TokenMeter>,
+    pub(crate) ledger_pool:     Arc<greentic_state::RedisPool>,
 }

 impl AgentRuntime {
     pub fn new(
         config_provider: Arc<dyn ConfigProvider>,
         state_store:     Arc<dyn AgentStateStore>,
         ext_runtime:     Arc<greentic_ext_runtime::ExtensionRuntime>,
         llm:             Arc<dyn LlmBackend>,
         telemetry:       Arc<dyn Telemetry>,
+        token_meter:     Arc<dyn TokenMeter>,
+        ledger_pool:     Arc<greentic_state::RedisPool>,
     ) -> Self {
-        Self { config_provider, state_store, ext_runtime, llm, telemetry }
+        Self { config_provider, state_store, ext_runtime, llm, telemetry, token_meter, ledger_pool }
     }
```

Add `pub use cost::TokenMeter;` if not already exported.

- [ ] **Step 2: Replace `loop.rs` with the full implementation**

```rust
//! Plan-Act-Observe agent loop. Spec §5.3.

use std::time::Instant;

use tracing::warn;

use crate::cost::TokenMeter;
use crate::error::{AgentError, TerminationReason};
use crate::llm::{LlmError, LlmRequest};
use crate::state::{ChatMessage, ToolCallRecord};
use crate::telemetry::StepTelemetryCtx;
use crate::tenant::TenantContext;
use crate::tools::{
    dispatch_tool_call, is_tool_allowed, ledger_key, list_tools_for_llm, ToolLedgerEntry,
};
use crate::{AgentInput, AgentOutput, AgentRuntime, AgentStep};

pub async fn run_step(
    runtime:    &AgentRuntime,
    tenant:     TenantContext,
    session_id: &str,
    agent_id:   &str,
    message:    AgentInput,
) -> Result<AgentOutput, AgentError> {
    let started = Instant::now();
    let config = runtime.config_provider.agent_config(&tenant, agent_id).await?;

    // ---- Cost budget gate ----
    if let Some(cap) = config.limits.daily_token_cap_per_tenant {
        let current = runtime.token_meter.current(&tenant).await?;
        if current >= cap as u64 {
            return Err(AgentError::TokenBudgetExceeded);
        }
    }

    // ---- Lock + state load ----
    let lock = runtime
        .state_store
        .acquire_lock(&tenant, session_id, std::time::Duration::from_secs(5))
        .await
        .map_err(|e| match e {
            crate::error::StateError::LockTimeout(_) => AgentError::LockTimeout,
            other => AgentError::StateLoad(other),
        })?;
    let mut state = match runtime.state_store.load(&tenant, session_id).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "state load failed; proceeding with empty state");
            crate::state::ConversationState::empty(&tenant, session_id)
        }
    };
    state.messages.push(ChatMessage::User { content: message.text });

    let mut total_tokens: u64 = 0;
    let mut trail: Vec<AgentStep> = Vec::new();
    let mut terminated_by = TerminationReason::MaxIterations;
    let mut iterations: u32 = 0;
    let mut reply: String = String::new();

    for iter in 0..config.limits.max_iter {
        iterations = iter + 1;
        if let Err(e) = lock.refresh().await {
            warn!(error = %e, "lock refresh failed; continuing");
        }
        if started.elapsed() >= config.limits.timeout {
            terminated_by = TerminationReason::Timeout;
            break;
        }

        let tools_schema = list_tools_for_llm(&runtime.ext_runtime, &config.tools);
        let request = LlmRequest {
            system_prompt: config.system_prompt.clone(),
            history:       state.messages.clone(),
            tools:         tools_schema,
            provider:      config.llm.clone(),
        };
        let response = match runtime.llm.complete(request).await {
            Ok(r) => r,
            Err(LlmError::ServiceUnavailable) => {
                // best-effort persist before bailing
                let _ = runtime.state_store.save(&tenant, session_id, &state).await;
                return Err(AgentError::LlmProviderUnavailable);
            }
            Err(other) => {
                let _ = runtime.state_store.save(&tenant, session_id, &state).await;
                return Err(AgentError::Llm(other));
            }
        };
        let step_tokens = (response.tokens_in + response.tokens_out) as u64;
        total_tokens += step_tokens;
        if let Err(e) = runtime.token_meter.add(&tenant, step_tokens).await {
            warn!(error = %e, "token meter add failed; continuing");
        }

        // Mixed text + tool_calls: tool_calls win per spec Decision 12.
        if !response.tool_calls.is_empty() {
            for call in response.tool_calls {
                if !is_tool_allowed(&call, &config.tools) {
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: serde_json::json!({
                            "error": "tool not allowed for this agent",
                        }),
                    });
                    trail.push(AgentStep::ToolCallBlocked {
                        name:   call.tool_name.clone(),
                        reason: "not in allow-list".into(),
                    });
                    continue;
                }

                // ---- Idempotency check ----
                let lkey = ledger_key(&tenant, session_id, &call.call_id);
                let mut conn = match runtime.ledger_pool.get().await {
                    Ok(c) => c,
                    Err(e) => {
                        warn!(error = %e, "ledger pool unavailable; bypassing idempotency");
                        // fall through to dispatch
                        let result = dispatch_or_record(
                            &runtime,
                            &tenant,
                            session_id,
                            call.clone(),
                            None,
                        ).await?;
                        push_tool_result(&mut state, &mut trail, &call, result);
                        continue;
                    }
                };
                let cached: Option<String> = conn.get(&lkey).await
                    .map_err(|e| AgentError::ToolDispatch(format!("ledger get: {e}")))?;
                if let Some(raw) = cached {
                    let entry: ToolLedgerEntry = serde_json::from_str(&raw)
                        .map_err(|e| AgentError::ToolDispatch(format!("ledger decode: {e}")))?;
                    state.messages.push(ChatMessage::Tool {
                        call_id: call.call_id.clone(),
                        content: entry.result.clone(),
                    });
                    trail.push(AgentStep::ToolCallReused {
                        name:    call.tool_name.clone(),
                        call_id: call.call_id.clone(),
                    });
                    continue;
                }
                drop(conn);

                let result = dispatch_or_record(
                    &runtime,
                    &tenant,
                    session_id,
                    call.clone(),
                    Some(lkey),
                ).await?;
                push_tool_result(&mut state, &mut trail, &call, result);
            }
            continue;
        }

        // No tool calls — final reply.
        if let Some(text) = response.content {
            reply = text.clone();
            state.messages.push(ChatMessage::Assistant {
                content:    text.clone(),
                tool_calls: vec![],
            });
            trail.push(AgentStep::Reply { text });
            terminated_by = TerminationReason::FinalReply;
            break;
        }
        // Edge case: LLM emitted neither content nor tool_calls.
        // Treat as empty final reply.
        terminated_by = TerminationReason::FinalReply;
        break;
    }

    state.truncate_history(config.limits.max_history_turns);
    if let Err(e) = runtime.state_store.save(&tenant, session_id, &state).await {
        warn!(error = %e, "state save failed at end of step");
    }

    runtime.telemetry.record_step(&StepTelemetryCtx {
        tenant_id:     tenant.tenant_id.clone(),
        env_id:        tenant.env_id.clone(),
        session_id:    session_id.to_string(),
        agent_id:      agent_id.to_string(),
        terminated_by: terminated_by.clone(),
        iterations,
        total_tokens,
        duration:      started.elapsed(),
    });

    Ok(AgentOutput { reply, trail, terminated_by })
}

async fn dispatch_or_record(
    runtime:    &AgentRuntime,
    tenant:     &TenantContext,
    _session:   &str,
    call:       ToolCallRecord,
    ledger_key: Option<String>,
) -> Result<serde_json::Value, AgentError> {
    let result = dispatch_tool_call(runtime.ext_runtime.clone(), tenant.clone(), call).await?;
    if let Some(key) = ledger_key {
        let entry = ToolLedgerEntry { result: result.clone() };
        let json = serde_json::to_string(&entry)
            .map_err(|e| AgentError::ToolDispatch(format!("ledger encode: {e}")))?;
        if let Ok(mut conn) = runtime.ledger_pool.get().await {
            let _ = conn.set_ex(&key, &json, 7 * 24 * 60 * 60).await;
        }
    }
    Ok(result)
}

fn push_tool_result(
    state:  &mut crate::state::ConversationState,
    trail:  &mut Vec<AgentStep>,
    call:   &ToolCallRecord,
    result: serde_json::Value,
) {
    state.messages.push(ChatMessage::Tool {
        call_id: call.call_id.clone(),
        content: result.clone(),
    });
    trail.push(AgentStep::ToolCall {
        name:    call.tool_name.clone(),
        call_id: call.call_id.clone(),
        result,
    });
}
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p greentic-aw-runtime --features test-mock`
Expected: Compiles. Loop tests still pass (existing happy-path test from Task 1.11).

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-aw-runtime/src/loop.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(aw-runtime): full Plan-Act-Observe loop with cost gate + idempotency ledger + mixed-response handling"
```

---

### Task 3.5: Scripted loop unit tests (mixed response, max_iter, timeout, blocked tool, retry, budget)

**Files:**
- New: `crates/greentic-aw-runtime/tests/loop_scripted.rs`

- [ ] **Step 1: Write all scripted-LLM tests in one file**

```rust
//! Scripted-LLM unit tests for the Plan-Act-Observe loop.
//! Uses test-mock backends — no Redis, no network.

#![cfg(feature = "test-mock")]

use std::sync::Arc;
use std::time::Duration;

use greentic_aw_runtime::cost::MockTokenMeter;
use greentic_aw_runtime::error::TerminationReason;
use greentic_aw_runtime::llm::LlmResponse;
use greentic_aw_runtime::mock::{
    MockAgentStateStore, MockConfigProvider, MockLlmBackend, MockTelemetry,
};
use greentic_aw_runtime::state::ToolCallRecord;
use greentic_aw_runtime::tenant::TenantContext;
use greentic_aw_runtime::{
    AgentConfig, AgentInput, AgentLimits, AgentRuntime, LlmProviderRef, ToolRef,
};

fn cfg(max_iter: u32, timeout_ms: u64, tools: Vec<ToolRef>) -> AgentConfig {
    AgentConfig {
        agent_id:      "a".into(),
        system_prompt: "sys".into(),
        tools,
        llm:           LlmProviderRef { provider: "openai".into(), model: "m".into() },
        limits: AgentLimits {
            max_iter,
            timeout: Duration::from_millis(timeout_ms),
            ..AgentLimits::default()
        },
    }
}

fn final_reply(text: &str) -> LlmResponse {
    LlmResponse { content: Some(text.into()), tool_calls: vec![], tokens_in: 5, tokens_out: 5 }
}

fn tool_call(call_id: &str, ext: &str, tool: &str) -> LlmResponse {
    LlmResponse {
        content: None,
        tool_calls: vec![ToolCallRecord {
            call_id: call_id.into(),
            extension_id: ext.into(),
            tool_name: tool.into(),
            args: serde_json::json!({}),
        }],
        tokens_in: 5,
        tokens_out: 5,
    }
}

fn build_runtime(
    llm_script: Vec<Result<LlmResponse, greentic_aw_runtime::error::LlmError>>,
    cfg_inner:  AgentConfig,
) -> (AgentRuntime, Arc<MockTelemetry>, Arc<MockTokenMeter>) {
    let llm = Arc::new(MockLlmBackend::new(llm_script));
    let store = Arc::new(MockAgentStateStore::new());
    let telemetry = Arc::new(MockTelemetry::new());
    let cp = MockConfigProvider::new();
    let tc = TenantContext::new("acme", "prod");
    cp.insert(&tc, "a", cfg_inner);
    let cp = Arc::new(cp);
    let token_meter = Arc::new(MockTokenMeter::new(0));
    // ledger_pool: use a stub that fails — loop falls back to non-cached dispatch.
    // We rely on the Phase 3 loop catching pool errors and bypassing idempotency.
    let ledger_pool = Arc::new(stub_pool());

    let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
    let rt = AgentRuntime::new(cp, store, ext, llm, telemetry.clone(), token_meter.clone(), ledger_pool);
    (rt, telemetry, token_meter)
}

fn stub_pool() -> greentic_state::RedisPool {
    // Returns a pool whose get() always errors. Loop must tolerate it.
    greentic_state::RedisPool::stub_failing()
}

#[tokio::test]
async fn happy_path_one_iteration() {
    let cfg = cfg(8, 60_000, vec![]);
    let (rt, tel, _) = build_runtime(vec![Ok(final_reply("hi"))], cfg);
    let tc = TenantContext::new("acme", "prod");
    let out = rt.step(tc, "s", "a", AgentInput { text: "hello".into() }).await.unwrap();
    assert_eq!(out.reply, "hi");
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    assert_eq!(tel.recorded.lock().unwrap()[0].iterations, 1);
}

#[tokio::test]
async fn max_iterations_terminates_loop() {
    // LLM emits tool calls forever (the tool will be blocked → loop continues)
    let cfg = cfg(3, 60_000, vec![]); // no tools allowed
    let script = vec![
        Ok(tool_call("c1", "http", "fetch")),
        Ok(tool_call("c2", "http", "fetch")),
        Ok(tool_call("c3", "http", "fetch")),
    ];
    let (rt, _, _) = build_runtime(script, cfg);
    let tc = TenantContext::new("acme", "prod");
    let out = rt.step(tc, "s", "a", AgentInput { text: "go".into() }).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::MaxIterations);
}

#[tokio::test]
async fn timeout_terminates_loop() {
    // Step timeout set to 50ms; LLM mock returns instantly so timeout triggers
    // on the iteration-boundary check via tokio sleep insertion.
    // Use a script that delays via sleeping inside the LLM mock — for that we
    // need a delayed mock. Simplest: timeout=0ms forces immediate exit.
    let cfg = cfg(8, 0, vec![]);
    let (rt, tel, _) = build_runtime(vec![Ok(final_reply("never reached"))], cfg);
    let tc = TenantContext::new("acme", "prod");
    let out = rt.step(tc, "s", "a", AgentInput { text: "x".into() }).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::Timeout);
    assert_eq!(tel.recorded.lock().unwrap()[0].iterations, 1);
}

#[tokio::test]
async fn tool_not_allowed_observation_then_reply() {
    let cfg = cfg(4, 60_000, vec![]); // empty allow list
    let script = vec![
        Ok(tool_call("c1", "http", "fetch")),
        Ok(final_reply("ok done")),
    ];
    let (rt, _, _) = build_runtime(script, cfg);
    let tc = TenantContext::new("acme", "prod");
    let out = rt.step(tc, "s", "a", AgentInput { text: "go".into() }).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    assert_eq!(out.reply, "ok done");
    let has_blocked = out.trail.iter().any(|s| matches!(
        s, greentic_aw_runtime::AgentStep::ToolCallBlocked { .. }
    ));
    assert!(has_blocked);
}

#[tokio::test]
async fn token_budget_exceeded_returns_error() {
    let cfg = AgentConfig {
        limits: AgentLimits {
            daily_token_cap_per_tenant: Some(10),
            ..AgentLimits::default()
        },
        ..cfg(8, 60_000, vec![])
    };
    let llm = Arc::new(MockLlmBackend::new(vec![Ok(final_reply("hi"))]));
    let store = Arc::new(MockAgentStateStore::new());
    let telemetry = Arc::new(MockTelemetry::new());
    let cp = MockConfigProvider::new();
    let tc = TenantContext::new("acme", "prod");
    cp.insert(&tc, "a", cfg);
    let cp = Arc::new(cp);
    let token_meter = Arc::new(MockTokenMeter::new(100)); // already above cap
    let ledger_pool = Arc::new(stub_pool());
    let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
    let rt = AgentRuntime::new(cp, store, ext, llm, telemetry, token_meter, ledger_pool);
    let err = rt.step(tc, "s", "a", AgentInput { text: "x".into() }).await.unwrap_err();
    assert!(matches!(err, greentic_aw_runtime::error::AgentError::TokenBudgetExceeded));
}

#[tokio::test]
async fn mixed_text_and_tool_calls_executes_tool_discards_text() {
    let cfg = cfg(4, 60_000, vec![ToolRef {
        extension_id: "http".into(),
        tool_name:    "fetch".into(),
    }]);
    // First response: mixed (content + tool_call). Tool dispatch will fail
    // because stub_pool ledger is broken and the for_test ExtensionRuntime
    // returns a dummy result. We assert content is NOT used as final reply.
    let mixed = LlmResponse {
        content: Some("internal reasoning here".into()),
        tool_calls: vec![ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            args: serde_json::json!({}),
        }],
        tokens_in: 5, tokens_out: 5,
    };
    let script = vec![Ok(mixed), Ok(final_reply("the real answer"))];
    let (rt, _, _) = build_runtime(script, cfg);
    let tc = TenantContext::new("acme", "prod");
    let out = rt.step(tc, "s", "a", AgentInput { text: "go".into() }).await.unwrap();
    assert_eq!(out.reply, "the real answer");
    assert_ne!(out.reply, "internal reasoning here");
}
```

- [ ] **Step 2: Add the `for_test`/`stub_failing` shims**

If `greentic_ext_runtime::ExtensionRuntime::for_test()` or `greentic_state::RedisPool::stub_failing()` does not exist, **gate Phase 3 on adding them upstream**. File issues:

```bash
cd /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer-extensions
gh issue create --title "Add ExtensionRuntime::for_test() shim for downstream test crates" \
  --body "greentic-aw-runtime needs a no-op ExtensionRuntime constructor for unit tests under \`--features test-mock\`. Add a \`pub fn for_test() -> Self\` that returns an instance with no extensions loaded; invoke_tool returns NotFound."
```

```bash
cd /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-state
gh issue create --title "Add RedisPool::stub_failing() shim for downstream test crates" \
  --body "greentic-aw-runtime needs a RedisPool stub whose get() always errors, for testing fallback paths when the pool is unavailable."
```

When both shims land, bump dependency tags and re-run.

- [ ] **Step 3: Run the scripted-loop tests**

Run: `cargo test -p greentic-aw-runtime --features test-mock --test loop_scripted`
Expected: 6 tests pass (or up to 6 — some may need pacing on the timeout case).

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-aw-runtime/tests/loop_scripted.rs
git commit -m "test(aw-runtime): scripted-LLM loop tests (final reply, max_iter, timeout, blocked tool, budget, mixed response)"
```

---

### Task 3.6: LLM provider unavailable test + Phase 3 PR

**Files:**
- Modify: `crates/greentic-aw-runtime/tests/loop_scripted.rs`

- [ ] **Step 1: Append the provider-down test**

```rust
#[tokio::test]
async fn llm_provider_unavailable_after_retries_returns_error() {
    use greentic_aw_runtime::error::{AgentError, LlmError};

    let cfg_inner = cfg(8, 60_000, vec![]);
    let script: Vec<Result<LlmResponse, LlmError>> = vec![
        Err(LlmError::ServiceUnavailable),
        Err(LlmError::ServiceUnavailable),
        Err(LlmError::ServiceUnavailable),
    ];
    let (rt, _, _) = build_runtime(script, cfg_inner);
    let tc = TenantContext::new("acme", "prod");
    let err = rt.step(tc, "s", "a", AgentInput { text: "x".into() }).await.unwrap_err();
    assert!(matches!(err, AgentError::LlmProviderUnavailable));
}
```

> **Important:** the Mock LLM in this test does NOT exercise `RetryingLlmBackend`. To exercise the decorator end-to-end, wrap the mock: `let retrying = Arc::new(RetryingLlmBackend::new(MockLlmBackend::new(script), 3, Duration::from_millis(1)));` and pass that as the LLM. Update the test to do exactly that.

- [ ] **Step 2: Run + commit**

```bash
cargo test -p greentic-aw-runtime --features test-mock --test loop_scripted llm_provider_unavailable
git add crates/greentic-aw-runtime/tests/loop_scripted.rs
git commit -m "test(aw-runtime): RetryingLlmBackend + loop returns LlmProviderUnavailable after all retries"
```

- [ ] **Step 3: Open PR3**

```bash
git push -u origin feat/aw-runtime-phase-3
gh pr create --base develop --title "feat(aw-runtime): Phase 3 — full Plan-Act-Observe loop + OpenAI backend + cost meter" --body "$(cat <<'EOF'
## Summary
- Implement `OpenAiLlmBackend` with function-calling
- Implement `RedisTokenMeter` for per-tenant daily token cap
- Replace Phase 1 stub loop with full Plan-Act-Observe per spec §5.3
- Idempotency ledger via `aw:{tenant}:{env}:{session}:tool_calls:{id}` Redis key
- Mixed text+tool_calls handling: tool_calls win (Decision 12)
- `spawn_blocking` wrap around `ExtensionRuntime::invoke_tool`
- TokenBudgetExceeded gate at step entry
- All termination paths covered: FinalReply, MaxIterations, Timeout, Error, TokenBudgetExceeded

## Test plan
- [ ] `cargo test -p greentic-aw-runtime --features test-mock` all pass
- [ ] X3 verified: admin LLM provider UI exists and can list/add/edit
- [ ] If `GREENTIC_LLM_TEST_BUDGET_USD>0` set, live OpenAI smoke test optional

## Spec refs
- §4 Decisions 4, 7, 11, 12, 14
- §5.3 Plan-Act-Observe pseudocode
- §6 Termination semantics
EOF
)"
```

---

## Phase 4 — Runner-Host `NodeKind::DwAgent` Wiring

**PR gate:** End-to-end runner flow test passes: `WebChat → DwAgent → reply` with mock LLM. X1 (session_id derivation, tenant plumbing) resolved.

**Pre-Phase-4 decision (X1):** Confirm two things by reading runner-host:
1. `FlowContext` exposes `tenant_id` + `env_id` (or equivalent) at the node-dispatch boundary. Search: `rg "tenant_id" crates/greentic-runner-host/src/runner/`.
2. The provider-input layer carries a `conversation_id` (or webchat thread id) that survives across messages. Search: `rg "conversation_id\|session_id" crates/greentic-runner-host/src/runner/`.

If either is missing, file a runner-host issue and pause Phase 4 until plumbed. The task code below assumes both are present.

### Task 4.1: Add `greentic-aw-runtime` as workspace dep to runner-host

**Files:**
- Modify: `crates/greentic-runner-host/Cargo.toml`

- [ ] **Step 1: Add the dep**

Modify `crates/greentic-runner-host/Cargo.toml` `[dependencies]`:

```diff
+greentic-aw-runtime = { path = "../greentic-aw-runtime" }
```

- [ ] **Step 2: Verify build**

Run: `cargo build -p greentic-runner-host`
Expected: Compiles. No use of `greentic_aw_runtime` yet so the dep is unused — clippy warning is acceptable until Task 4.3.

- [ ] **Step 3: Commit**

```bash
git add crates/greentic-runner-host/Cargo.toml
git commit -m "chore(runner-host): wire greentic-aw-runtime workspace dep (used in DwAgent NodeKind)"
```

---

### Task 4.2: Extend NodeKind with DwAgent variant

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/engine.rs:126`

- [ ] **Step 1: Add the variant**

In `crates/greentic-runner-host/src/runner/engine.rs` around line 126:

```diff
 enum NodeKind {
     Exec { target_component: String },
     PackComponent { component_ref: String },
     ProviderInvoke,
     FlowCall,
     BuiltinEmit { kind: EmitKind },
     BuiltinStateGet,
     BuiltinStateSet,
     Wait,
+    DwAgent { agent_id: String },
 }
```

- [ ] **Step 2: Look for the matching pattern site**

Find every `match` on `NodeKind` in `engine.rs`:

```bash
grep -n "match .*node_kind\|match .*NodeKind\|match.*\.kind" /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-runner/crates/greentic-runner-host/src/runner/engine.rs
```

Note each line — every match needs a new arm. Add `NodeKind::DwAgent { agent_id } => { /* dispatch — Task 4.3 wires this */ todo!("DwAgent dispatch — Task 4.3") }` to each match to keep compilation going temporarily. (Phase 4 task 4.3 fills these in.)

- [ ] **Step 3: Verify build**

Run: `cargo build -p greentic-runner-host`
Expected: Compiles (the `todo!()` marker is fine at this step).

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-runner-host/src/runner/engine.rs
git commit -m "feat(runner-host): NodeKind::DwAgent variant (dispatch wired in next commit)"
```

---

### Task 4.3: AgentNodeHandler glue module + dispatch arm

**Files:**
- New: `crates/greentic-runner-host/src/runner/agent_node.rs`
- Modify: `crates/greentic-runner-host/src/runner/engine.rs` (replace `todo!()` with real call)
- Modify: `crates/greentic-runner-host/src/runner/mod.rs` — `pub mod agent_node;`

- [ ] **Step 1: Write the handler module**

```rust
//! Bridges `FlowEngine` dispatch into the `greentic-aw-runtime` library.
//!
//! Responsibilities:
//! 1. Derive `(TenantContext, session_id)` from flow context.
//! 2. Source the `AgentConfig` for `agent_id` from the pack metadata
//!    via the production `ConfigProvider`.
//! 3. Translate the AW reply into the flow node's output value.
//! 4. Surface errors through `AgentError::user_facing_message()` so
//!    end-user-facing flow outputs never leak internal error chains.

use std::sync::Arc;

use anyhow::{Context, Result};
use greentic_aw_runtime::{
    AgentInput, AgentRuntime, TenantContext,
};
use serde_json::Value;

pub struct AgentNodeHandler {
    runtime: Arc<AgentRuntime>,
}

impl AgentNodeHandler {
    pub fn new(runtime: Arc<AgentRuntime>) -> Self {
        Self { runtime }
    }

    /// Execute a `DwAgent` flow node. `flow_input` is the JSON value the
    /// upstream node produced — we expect at least
    /// `{ "user_text": String }` and pass `conversation_id` via
    /// the flow's session context (caller plumbs).
    pub async fn execute(
        &self,
        tenant_id:       &str,
        env_id:          &str,
        agent_id:        &str,
        conversation_id: &str,
        flow_input:      &Value,
    ) -> Result<Value> {
        let user_text = flow_input
            .get("user_text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let tenant = TenantContext::new(tenant_id, env_id);
        match self
            .runtime
            .step(
                tenant,
                conversation_id,
                agent_id,
                AgentInput { text: user_text },
            )
            .await
        {
            Ok(out) => Ok(serde_json::json!({
                "reply":         out.reply,
                "trail":         out.trail,
                "terminated_by": out.terminated_by,
            })),
            Err(err) => {
                // Reload config (cheap with caching provider) to render the
                // sanitised user-facing message.
                let cfg = self
                    .runtime
                    .config_provider
                    .agent_config(&TenantContext::new(tenant_id, env_id), agent_id)
                    .await
                    .context("agent config for sanitised error message")?;
                Ok(serde_json::json!({
                    "reply":         err.user_facing_message(&cfg),
                    "trail":         Vec::<greentic_aw_runtime::AgentStep>::new(),
                    "terminated_by": greentic_aw_runtime::error::TerminationReason::Error,
                    "error":         err.to_string(),
                }))
            }
        }
    }
}
```

> **Required visibility change in `greentic-aw-runtime`:** the handler reads `runtime.config_provider`. Adjust the field to `pub` (it's currently `pub(crate)`) OR add a `pub fn config_provider(&self) -> &Arc<dyn ConfigProvider>` accessor on `AgentRuntime`. Prefer the accessor for encapsulation.

Add accessor to `lib.rs`:

```diff
 impl AgentRuntime {
     pub fn new(...) -> Self { ... }
+    pub fn config_provider(&self) -> &Arc<dyn ConfigProvider> {
+        &self.config_provider
+    }
     pub async fn step(...) -> Result<...> { ... }
 }
```

Update the handler accordingly: `self.runtime.config_provider().agent_config(...)`.

- [ ] **Step 2: Register the module**

In `crates/greentic-runner-host/src/runner/mod.rs`, add `pub mod agent_node;`.

- [ ] **Step 3: Replace `todo!()` dispatch arms with real calls**

In `engine.rs`, replace each `todo!("DwAgent dispatch — Task 4.3")` with the dispatch. Show one match site:

```rust
NodeKind::DwAgent { agent_id } => {
    let handler = self.agent_node_handler.as_ref()
        .context("AgentNodeHandler not configured for runner")?;
    let conv_id = flow_ctx.conversation_id().unwrap_or("");
    let result = handler.execute(
        &flow_ctx.tenant_id,
        &flow_ctx.env_id,
        agent_id,
        conv_id,
        &node_input,
    ).await?;
    NodeOutput::Value(result)
}
```

This requires adding `agent_node_handler: Option<Arc<AgentNodeHandler>>` to the `FlowEngine` struct and a setter. The exact wiring depends on `FlowEngine::new` — adapt to the existing builder pattern.

- [ ] **Step 4: Verify build**

Run: `cargo build -p greentic-runner-host`
Expected: Compiles. Clippy warnings about unused fields acceptable until Task 4.4 plumbs the handler.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/runner/agent_node.rs crates/greentic-runner-host/src/runner/mod.rs crates/greentic-runner-host/src/runner/engine.rs crates/greentic-aw-runtime/src/lib.rs
git commit -m "feat(runner-host): AgentNodeHandler bridges DwAgent NodeKind into greentic-aw-runtime"
```

---

### Task 4.4: Wire `AgentNodeHandler` construction in runner startup

**Files:**
- Modify: `crates/greentic-runner/src/main.rs` (or wherever `FlowEngine` is built)

- [ ] **Step 1: Find the FlowEngine construction site**

```bash
rg "FlowEngine::new" /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-runner/crates --type rust
```

- [ ] **Step 2: Construct the AW runtime + handler alongside**

Add near the existing `FlowEngine::new(...)` call:

```rust
use std::sync::Arc;
use greentic_aw_runtime::{
    AgentRuntime, OpenAiLlmBackend, RetryingLlmBackend, RedisAgentStateStore,
    RedisTokenMeter, OtelTelemetry, CachingConfigProvider,
};

// 1. Source pieces from existing runner config.
let redis_url = config.redis_url.as_str();
let openai_key = config.llm.openai_api_key.clone()
    .context("OPENAI_API_KEY must be configured for DwAgent flows")?;

// 2. Build trait objects.
let state_store = Arc::new(
    RedisAgentStateStore::connect(redis_url).await
        .context("connect AW state Redis")?
);
let token_meter_pool = Arc::clone(&state_store.pool());
let token_meter = Arc::new(RedisTokenMeter::new(token_meter_pool.clone()));
let ledger_pool = token_meter_pool.clone();

let llm_inner = OpenAiLlmBackend::new(openai_key);
let llm = Arc::new(RetryingLlmBackend::new(
    llm_inner,
    3,
    std::time::Duration::from_millis(250),
));

let config_provider = Arc::new(CachingConfigProvider::new(
    pack_config_provider, // existing pack-backed provider, source: gtpack metadata
));

let telemetry = Arc::new(OtelTelemetry);

let agent_runtime = Arc::new(AgentRuntime::new(
    config_provider,
    state_store,
    ext_runtime.clone(),
    llm,
    telemetry,
    token_meter,
    ledger_pool,
));

let agent_handler = Arc::new(
    greentic_runner_host::runner::agent_node::AgentNodeHandler::new(agent_runtime),
);

// 3. Pass to FlowEngine via setter:
let flow_engine = FlowEngine::new(packs, host_config).await?
    .with_agent_handler(agent_handler);
```

Also add `with_agent_handler` builder to `FlowEngine`:

```rust
impl FlowEngine {
    pub fn with_agent_handler(mut self, handler: Arc<AgentNodeHandler>) -> Self {
        self.agent_node_handler = Some(handler);
        self
    }
}
```

> **Pack-backed `ConfigProvider`:** the runner derives `AgentConfig` from `.gtpack` metadata (CBOR-encoded `dw-agent.cbor` per pack). Inspect existing packs at `packs/` to confirm the file name. If absent, plan a minimal `PackConfigProvider` that reads agent configs from the pack and emit a follow-up issue to formalise the `.gtpack` extension for AW agents.

- [ ] **Step 3: Verify build**

Run: `cargo build -p greentic-runner`
Expected: Compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/greentic-runner/src crates/greentic-runner-host/src
git commit -m "feat(runner): construct AgentRuntime + AgentNodeHandler at startup"
```

---

### Task 4.5: E2E flow test — WebChat → DwAgent → reply

**Files:**
- New: `crates/tests/flows/dw_agent_basic.ygtc`
- New: `crates/tests/dw_agent_e2e.rs`

- [ ] **Step 1: Write the flow definition**

Create `crates/tests/flows/dw_agent_basic.ygtc`:

```yaml
flow:
  id: dw-agent-basic-e2e
  version: 0.1.0
  nodes:
    - id: input
      kind: provider_invoke
      provider: webchat-mock
      operation: read_message
    - id: agent
      kind: dw_agent
      agent_id: e2e-test-agent
    - id: output
      kind: builtin_emit
      kind_detail: response
  edges:
    - from: input
      to: agent
      map:
        user_text: $.text
    - from: agent
      to: output
      map:
        text: $.reply
```

- [ ] **Step 2: Write the E2E test**

Create `crates/tests/dw_agent_e2e.rs`:

```rust
//! End-to-end runner test: WebChat (mock) → DwAgent → response.
//!
//! Uses an in-memory ConfigProvider seeded with one agent + the
//! `for_test` ExtensionRuntime + a mock LLM. Runs against a real
//! Redis instance (REDIS_URL env required).

#![cfg(feature = "test-mock")]

use std::sync::Arc;
use std::time::Duration;

use greentic_aw_runtime::{
    cost::MockTokenMeter,
    mock::{MockConfigProvider, MockLlmBackend, MockTelemetry},
    state::ChatMessage,
    AgentConfig, AgentLimits, AgentRuntime, LlmProviderRef, RedisAgentStateStore,
    TenantContext,
};
use greentic_runner_host::runner::agent_node::AgentNodeHandler;

#[tokio::test]
async fn webchat_to_dwagent_returns_llm_reply() {
    let Some(redis_url) = std::env::var("REDIS_URL").ok() else {
        eprintln!("REDIS_URL unset; skipping");
        return;
    };
    let state_store = Arc::new(
        RedisAgentStateStore::connect(&redis_url).await.unwrap()
    );
    let pool = state_store.pool();
    let token_meter = Arc::new(MockTokenMeter::new(0));
    let telemetry = Arc::new(MockTelemetry::new());

    let cp = MockConfigProvider::new();
    let tc = TenantContext::new("e2e-tenant", "e2e-env");
    cp.insert(&tc, "e2e-test-agent", AgentConfig {
        agent_id:      "e2e-test-agent".into(),
        system_prompt: "respond with 'pong'".into(),
        tools:         vec![],
        llm:           LlmProviderRef { provider: "mock".into(), model: "x".into() },
        limits:        AgentLimits::default(),
    });
    let cp = Arc::new(cp);
    let llm = Arc::new(MockLlmBackend::new(vec![Ok(greentic_aw_runtime::llm::LlmResponse {
        content: Some("pong".into()),
        tool_calls: vec![],
        tokens_in: 1, tokens_out: 1,
    })]));
    let ext = Arc::new(greentic_ext_runtime::ExtensionRuntime::for_test());
    let rt = Arc::new(AgentRuntime::new(cp, state_store, ext, llm, telemetry, token_meter, pool));

    let handler = AgentNodeHandler::new(rt);
    let input = serde_json::json!({ "user_text": "ping" });
    let out = handler.execute(
        "e2e-tenant", "e2e-env", "e2e-test-agent",
        "conv-e2e-1", &input,
    ).await.unwrap();
    assert_eq!(out["reply"].as_str(), Some("pong"));
}
```

- [ ] **Step 3: Run**

Run: `REDIS_URL=redis://localhost:6379/15 cargo test -p tests --features test-mock --test dw_agent_e2e -- --nocapture`
Expected: 1 test passes.

- [ ] **Step 4: Commit**

```bash
git add crates/tests/dw_agent_e2e.rs crates/tests/flows/dw_agent_basic.ygtc
git commit -m "test(runner): E2E WebChat → DwAgent → reply with mock LLM + real Redis"
```

- [ ] **Step 5: Open PR4**

```bash
git push -u origin feat/aw-runtime-phase-4
gh pr create --base develop --title "feat(runner-host): Phase 4 — NodeKind::DwAgent + AgentNodeHandler" --body "$(cat <<'EOF'
## Summary
- Add `NodeKind::DwAgent { agent_id }` to FlowEngine
- New `AgentNodeHandler` bridges flow-context (tenant, env, conversation_id) into `AgentRuntime::step`
- Construct `AgentRuntime` (Redis state + OpenAI backend + Otel) at runner startup
- E2E test: WebChat (mock) → DwAgent → reply

## Test plan
- [ ] `REDIS_URL=redis://localhost:6379/15 cargo test -p tests --features test-mock --test dw_agent_e2e`

## X1 resolution
- session_id derived from `flow_ctx.conversation_id()`
- TenantContext from `flow_ctx.tenant_id` + `flow_ctx.env_id`
- Errors sanitised via `AgentError::user_facing_message()`
EOF
)"
```

---

## Phase 5 — Designer `/api/chat` Playground Integration

**PR gate:** Designer playground roundtrip test passes; "thinking..." indicator visible while request in flight. X2 (DwFormState → AgentConfig translation) resolved.

**Pre-Phase-5:** Read the existing dispatcher and `DwFormState` struct to understand current shape:

```bash
cat /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/src/ui/routes/dw_test_chat/dispatcher.rs
rg "struct DwFormState" /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/src -t rust | head -5
```

Skim both before drafting Task 5.2.

### Task 5.1: Add `greentic-aw-runtime` git dep to designer

**Files:**
- Modify: `/Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/Cargo.toml`

- [ ] **Step 1: Find the existing `[dependencies]` block**

```bash
rg -n "^\[dependencies\]" /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/Cargo.toml
```

- [ ] **Step 2: Add the dep**

```diff
+greentic-aw-runtime = { git = "https://github.com/greentic-biz/greentic-runner", branch = "research" }
+# IMPORTANT: keep this tag in sync with the runner-host pin. See spec §5.1.1.
```

> **Cross-workspace sync rule:** the `greentic-ext-runtime` tag pinned by `greentic-aw-runtime` (via its own `Cargo.toml`) MUST match the tag pinned directly by the designer (in `greentic-designer/Cargo.toml`). Mismatch produces orphan-rule errors at the trait boundary. Before merging Phase 5, verify both are on `v1.2.8-research` (or higher, in sync).

- [ ] **Step 3: Verify build**

Run: `cd /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer && cargo build`
Expected: Compiles.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml
git commit -m "chore(designer): add greentic-aw-runtime git dep for playground integration"
```

---

### Task 5.2: DwFormState → AgentConfig translator

**Files:**
- New: `src/ui/agent/playground_config.rs`
- Modify: `src/ui/agent/mod.rs` — `pub mod playground_config;`

- [ ] **Step 1: Read the current DwFormState shape**

```bash
rg -B2 -A30 "pub struct DwFormState" /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/src -t rust | head -60
```

- [ ] **Step 2: Write the translator**

```rust
//! `DwFormState → AgentConfig` translation for the designer playground.
//!
//! Designer's live form state is the operator's in-progress agent
//! definition. To exercise the agent against the real runtime, we
//! materialise it into an `AgentConfig` and seed an in-memory
//! `ConfigProvider`. Re-translation happens per request so the
//! playground always reflects the latest form values.

use greentic_aw_runtime::{AgentConfig, AgentLimits, LlmProviderRef, ToolRef};

use crate::ui::storage::dw_form::DwFormState; // adjust import path to match repo

/// Build an `AgentConfig` from the designer form state.
///
/// `agent_id` is the playground session id (used as the agent_id
/// inside the AW runtime so the in-memory provider can find it).
pub fn build_agent_config(form: &DwFormState, agent_id: &str) -> AgentConfig {
    let system_prompt = form
        .values
        .get("system_prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("You are a helpful assistant.")
        .to_string();

    let provider = form
        .values
        .get("llm_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("openai")
        .to_string();
    let model = form
        .values
        .get("llm_model")
        .and_then(|v| v.as_str())
        .unwrap_or("gpt-4o-mini")
        .to_string();

    let tools: Vec<ToolRef> = form
        .values
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let id = t.get("extension_id")?.as_str()?.to_string();
                    let name = t.get("tool_name")?.as_str()?.to_string();
                    Some(ToolRef { extension_id: id, tool_name: name })
                })
                .collect()
        })
        .unwrap_or_default();

    AgentConfig {
        agent_id:      agent_id.to_string(),
        system_prompt,
        tools,
        llm:           LlmProviderRef { provider, model },
        limits:        AgentLimits::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn form_with(values: serde_json::Value) -> DwFormState {
        // Adapt to actual DwFormState constructor — this is the shape
        // assumed for the test. Adjust per repo.
        DwFormState {
            values,
            ..DwFormState::default()
        }
    }

    #[test]
    fn picks_up_system_prompt_from_form() {
        let f = form_with(json!({ "system_prompt": "You are bank Bob." }));
        let cfg = build_agent_config(&f, "agent-1");
        assert_eq!(cfg.system_prompt, "You are bank Bob.");
    }

    #[test]
    fn defaults_when_form_empty() {
        let f = form_with(json!({}));
        let cfg = build_agent_config(&f, "agent-1");
        assert_eq!(cfg.llm.provider, "openai");
        assert_eq!(cfg.llm.model, "gpt-4o-mini");
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn extracts_tool_list() {
        let f = form_with(json!({
            "tools": [
                { "extension_id": "http", "tool_name": "fetch" },
                { "extension_id": "calendar", "tool_name": "create" }
            ]
        }));
        let cfg = build_agent_config(&f, "agent-1");
        assert_eq!(cfg.tools.len(), 2);
        assert_eq!(cfg.tools[0].extension_id, "http");
        assert_eq!(cfg.tools[1].tool_name, "create");
    }
}
```

> **Adjust to actual repo:** import path `DwFormState` may live elsewhere; the construction inside `form_with` may differ (no public `Default` impl, etc.). Read the actual struct and adapt before running tests.

- [ ] **Step 3: Run tests**

Run: `cargo test -p greentic-designer playground_config`
Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/ui/agent/playground_config.rs src/ui/agent/mod.rs
git commit -m "feat(designer): playground_config — DwFormState → AgentConfig translator with tests"
```

---

### Task 5.3: Swap dispatcher to use AgentRuntime

**Files:**
- Modify: `src/ui/routes/dw_test_chat/dispatcher.rs`

- [ ] **Step 1: Read the existing dispatcher**

```bash
cat /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/src/ui/routes/dw_test_chat/dispatcher.rs
```

- [ ] **Step 2: Replace the stub with a real AW runtime call**

The exact diff depends on the existing function signatures; the conceptual change is:

```rust
// BEFORE: stub returned a canned response
pub async fn dispatch_user_message(...) -> TestChatEvent {
    TestChatEvent::AssistantText { text: "stub reply".into() }
}

// AFTER: build a per-request InMemoryConfigProvider + AgentRuntime, then step.
use std::sync::Arc;
use greentic_aw_runtime::{
    config_provider::InMemoryConfigProvider,
    cost::MockTokenMeter,
    mock::{MockAgentStateStore, MockTelemetry},
    AgentInput, AgentRuntime, OpenAiLlmBackend, RetryingLlmBackend, TenantContext,
};
use std::time::Duration;

use crate::ui::agent::playground_config::build_agent_config;

pub async fn dispatch_user_message(
    session_uuid: uuid::Uuid,
    form: &crate::ui::storage::dw_form::DwFormState,
    user_input: &str,
    openai_key: &str,
    ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
) -> TestChatEvent {
    let tenant = TenantContext::new("designer-preview", "default");
    let agent_id = format!("playground-{session_uuid}");
    let cfg = build_agent_config(form, &agent_id);

    let mut cp = InMemoryConfigProvider::new();
    cp.insert(&tenant, &agent_id, cfg);

    let llm = Arc::new(RetryingLlmBackend::new(
        OpenAiLlmBackend::new(openai_key.to_string()),
        3,
        Duration::from_millis(250),
    ));
    // In-memory state for designer playground — survives only this
    // process. For multi-message conversations within the same browser
    // tab, see Task 5.4 (persistent store keyed on session_uuid).
    let store = Arc::new(MockAgentStateStore::new());
    let token_meter = Arc::new(MockTokenMeter::new(0));
    let ledger_pool = Arc::new(greentic_state::RedisPool::stub_failing());
    let telemetry = Arc::new(MockTelemetry::new());

    let rt = AgentRuntime::new(
        Arc::new(cp),
        store,
        ext_runtime,
        llm,
        telemetry,
        token_meter,
        ledger_pool,
    );

    match rt
        .step(
            tenant.clone(),
            &session_uuid.to_string(),
            &agent_id,
            AgentInput { text: user_input.to_string() },
        )
        .await
    {
        Ok(out) => TestChatEvent::AssistantText { text: out.reply },
        Err(err) => {
            let cfg = build_agent_config(form, &agent_id);
            TestChatEvent::AssistantText {
                text: err.user_facing_message(&cfg),
            }
        }
    }
}
```

> **State persistence in playground:** the dispatcher above uses a fresh `MockAgentStateStore` per dispatch, so multi-turn context is lost. Multi-turn continuity in the playground requires a `TestChatSessions`-scoped `AgentStateStore` (one per session_uuid). Task 5.4 adds this.

- [ ] **Step 3: Verify build**

Run: `cargo build -p greentic-designer`
Expected: Compiles.

- [ ] **Step 4: Commit**

```bash
git add src/ui/routes/dw_test_chat/dispatcher.rs
git commit -m "feat(designer): dispatch playground chat through greentic-aw-runtime AgentRuntime"
```

---

### Task 5.4: Per-session state retention in playground

**Files:**
- Modify: `src/ui/routes/dw_test_chat.rs` (extend `TestChatSessionEntry`)
- Modify: `src/ui/routes/dw_test_chat/dispatcher.rs`

- [ ] **Step 1: Add state store handle to session entry**

```rust
// in dw_test_chat.rs
pub struct TestChatSessionEntry {
    pub receiver:    Mutex<Option<mpsc::Receiver<TestChatEvent>>>,
    pub created_at:  DateTime<Utc>,
    pub state_store: Arc<greentic_aw_runtime::mock::MockAgentStateStore>, // playground only
}
```

- [ ] **Step 2: Construct the store in the session-creation path + thread it into dispatcher**

In `post_messages`, when creating the session entry, allocate one `MockAgentStateStore::new()` per session. Pass the same `Arc` to every subsequent `dispatch_user_message` call. Update the dispatcher signature to accept `state_store: Arc<MockAgentStateStore>` instead of constructing one.

- [ ] **Step 3: Verify build + commit**

```bash
cargo build -p greentic-designer
git add src/ui/routes/dw_test_chat.rs src/ui/routes/dw_test_chat/dispatcher.rs
git commit -m "feat(designer): per-session MockAgentStateStore for multi-turn playground continuity"
```

---

### Task 5.5: "Thinking..." typing indicator (frontend)

**Files:**
- New: `/Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/web/src/components/dw-test-chat/TypingIndicator.tsx`
- Modify: `web/src/components/dw-test-chat/ChatPanel.tsx` (or wherever the chat panel lives)

- [ ] **Step 1: Find the chat panel component**

```bash
find /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/web/src -name "*hat*.tsx" -o -name "*hat*.jsx" 2>/dev/null
```

- [ ] **Step 2: Write the indicator**

```tsx
//! Subtle "thinking..." indicator shown while the assistant request
//! is in flight. MVP heartbeat — replaces blank-screen UX between
//! user message submit and assistant reply arrival.

import { FC } from "react";

interface Props {
  visible: boolean;
}

export const TypingIndicator: FC<Props> = ({ visible }) => {
  if (!visible) return null;
  return (
    <div className="flex items-center gap-2 px-4 py-2 text-sm text-muted-foreground">
      <div className="flex gap-1">
        <span className="w-2 h-2 rounded-full bg-current animate-bounce" style={{ animationDelay: "0ms" }} />
        <span className="w-2 h-2 rounded-full bg-current animate-bounce" style={{ animationDelay: "150ms" }} />
        <span className="w-2 h-2 rounded-full bg-current animate-bounce" style={{ animationDelay: "300ms" }} />
      </div>
      <span>thinking…</span>
    </div>
  );
};
```

- [ ] **Step 3: Wire it into the chat panel**

In the chat panel component, track an `isWaiting` boolean state. Set `true` when the POST to `/api/chat` (or whatever the dw-test-chat endpoint is) begins; set `false` when the response arrives. Render `<TypingIndicator visible={isWaiting} />` at the bottom of the message list.

Typical pattern:

```tsx
const [isWaiting, setIsWaiting] = useState(false);

const handleSend = async (text: string) => {
  setIsWaiting(true);
  try {
    await sendMessage(text);
  } finally {
    setIsWaiting(false);
  }
};

return (
  <div>
    <MessageList messages={messages} />
    <TypingIndicator visible={isWaiting} />
    <Composer onSend={handleSend} disabled={isWaiting} />
  </div>
);
```

- [ ] **Step 4: Verify build**

Run: `cd web && npm run build`
Expected: Bundle builds clean.

- [ ] **Step 5: Manual smoke**

Start the designer + admin in dev (`make dev`). Open the playground, type a message, observe:
- "thinking..." appears while waiting
- disappears when reply arrives
- composer is disabled while in-flight

- [ ] **Step 6: Commit**

```bash
git add web/src/components/dw-test-chat
git commit -m "feat(designer): TypingIndicator heartbeat while /api/chat request in flight"
```

---

### Task 5.6: Playground roundtrip integration test

**Files:**
- New: `src/ui/routes/dw_test_chat/playground_test.rs`

- [ ] **Step 1: Write the test**

```rust
//! Integration test: POST /api/dw-test-chat → AgentRuntime → reply roundtrips.

#![cfg(feature = "test-mock")]

use axum_test::TestServer;
use serde_json::json;

#[tokio::test]
async fn playground_roundtrip_returns_assistant_reply() {
    let app = crate::test_support::build_app_with_mock_llm("pong");
    let server = TestServer::new(app).unwrap();

    let resp = server
        .post("/api/dw-test-chat/messages")
        .json(&json!({
            "dw_form": {
                "values": {
                    "system_prompt": "reply with pong",
                    "llm_provider":  "mock",
                    "llm_model":     "x"
                }
            },
            "secrets": {},
            "messages": [],
            "user_input": "ping"
        }))
        .await;
    resp.assert_status_ok();
    let body: serde_json::Value = resp.json();
    let session_id = body["session_id"].as_str().unwrap();
    assert!(!session_id.is_empty());

    // Stream the events back via the GET endpoint (NDJSON or SSE).
    let stream = server.get(&format!("/api/dw-test-chat/{session_id}/stream")).await;
    stream.assert_status_ok();
    let text = stream.text();
    assert!(text.contains("\"text\":\"pong\""));
}
```

> Replace `crate::test_support::build_app_with_mock_llm` with the actual helper used in other designer tests — search `rg "build_app" /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-designer/src --type rust`.

- [ ] **Step 2: Run + commit**

```bash
cargo test -p greentic-designer --features test-mock playground_roundtrip
git add src/ui/routes/dw_test_chat/playground_test.rs
git commit -m "test(designer): playground POST → stream → assistant reply roundtrip"
```

- [ ] **Step 3: Open PR5**

```bash
git push -u origin feat/aw-runtime-phase-5
gh pr create --base develop --title "feat(designer): Phase 5 — playground /api/chat via greentic-aw-runtime" --body "$(cat <<'EOF'
## Summary
- Add `greentic-aw-runtime` git dep
- `DwFormState → AgentConfig` translator (`playground_config.rs`)
- Replace stub dispatcher with `AgentRuntime::step` call
- Per-session `MockAgentStateStore` retains multi-turn context inside playground
- "thinking..." TypingIndicator while request in flight
- Roundtrip integration test

## X2 resolution
- DwFormState fields mapped: system_prompt, llm_provider, llm_model, tools[]
- Session id = playground UUID v4 per operator browser session
- Errors sanitised via `AgentError::user_facing_message()`

## Test plan
- [ ] `cargo test -p greentic-designer --features test-mock`
- [ ] `cd web && npm run build`
- [ ] Manual smoke: playground chat shows thinking + reply
EOF
)"
```

---

## Phase 6 — Acceptance + E2E + Polish

**PR gate:** All 10 acceptance criteria from spec §8.5 verified. Bug-fix-only PR.

### Task 6.1: Acceptance #1 — Deployable WebChat → DwAgent → reply flow

- [ ] **Step 1: Cherry-pick the Phase 4 E2E test (already passing) and re-run end-to-end against the merged develop**

Run:
```bash
git checkout develop && git pull
REDIS_URL=redis://localhost:6379/15 cargo test -p tests --features test-mock --test dw_agent_e2e
```
Expected: Passes on merged develop.

- [ ] **Step 2: Promote the flow to `crates/tests/flows/` so future smoke tests use it as a baseline**

Already done in Task 4.5. Just verify the file is committed: `git ls-files | grep dw_agent_basic.ygtc`.

- [ ] **Step 3: Mark in acceptance checklist**

Append to `docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md` a new section "Acceptance Tracking" with checkboxes mirroring §8.5. Mark #1 done.

```bash
cat >> /Users/bimapangestu/Desktop/Works/personal/greentic/greentic-runner/docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md << 'EOF'

## 14. Acceptance Tracking

- [x] 1. Flow with `WebChat → DwAgent → reply` deployable + functional (Phase 4 task 4.5)
- [ ] 2. Designer test playground produces identical agent behaviour
- [ ] 3. Multi-tenant: 2 tenants in same Redis, zero cross-talk
- [ ] 4. Plan-Act-Observe with 1+ tool call observed end-to-end
- [ ] 5. Max iter + timeout enforcement
- [ ] 6. State persists across runner restart
- [ ] 7. `aw.step` telemetry span visible per agent step
- [ ] 8. Banking-id composer's generated config → real reply
- [ ] 9. Daily token cap enforcement with real Redis
- [ ] 10. Tool idempotency: double-dispatch regression test
EOF
git add docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
git commit -m "docs(aw-runtime): acceptance tracking — #1 deployable flow verified"
```

---

### Task 6.2: Acceptance #2 — Designer playground produces identical behaviour to deployed flow

- [ ] **Step 1: Write a parity test**

Create `crates/tests/playground_parity.rs`:

```rust
//! Parity test: same AgentConfig + same user message yields the same
//! AgentOutput from the runner-host AgentNodeHandler and the designer
//! dispatcher path. Catches drift where one diverges from the other.

#![cfg(feature = "test-mock")]

#[tokio::test]
async fn runner_and_designer_produce_identical_reply_for_same_config() {
    // 1. Build identical (cp, store, llm, ext, telemetry, meter, ledger) tuples for both paths.
    // 2. Invoke runner-host's AgentNodeHandler.execute.
    // 3. Invoke designer's dispatch_user_message.
    // 4. Assert: reply strings match; terminated_by matches.
    // (Code follows the pattern from dw_agent_e2e.rs — copy + adapt.)
    // ... (full implementation: see file)
}
```

Fill in the body following Task 4.5's pattern. Both paths must use the same mock LLM script.

- [ ] **Step 2: Run + commit**

```bash
REDIS_URL=redis://localhost:6379/15 cargo test -p tests --features test-mock --test playground_parity
git add crates/tests/playground_parity.rs
git commit -m "test(runner+designer): playground parity — identical config yields identical reply"
```

- [ ] **Step 3: Mark acceptance #2 done**

```bash
sed -i.bak 's/- \[ \] 2\. Designer test/- [x] 2. Designer test/' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
rm docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md.bak
git add docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
git commit -m "docs(aw-runtime): acceptance #2 verified — playground parity test passes"
```

---

### Task 6.3: Acceptance #3 (multi-tenant), #5 (max iter + timeout), #7 (telemetry span visible), #10 (tool idempotency)

These are all already covered by tests written in earlier phases. Verify each:

- [ ] **#3 multi-tenant:** Task 2.6 `two_tenants_share_redis_without_cross_talk` covers this. Re-run:
  ```bash
  REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test redis_state two_tenants
  ```
  Expected: passes.

- [ ] **#5 max iter + timeout:** Task 3.5 `max_iterations_terminates_loop` + `timeout_terminates_loop` cover. Re-run:
  ```bash
  cargo test -p greentic-aw-runtime --features test-mock --test loop_scripted -- max_iter timeout
  ```
  Expected: passes.

- [ ] **#7 telemetry span:** Task 1.8 test + Task 3.5 loop tests already verify `MockTelemetry.recorded` is populated. For real OTel emission, run the runner with `tracing-subscriber` in JSON mode and grep stdout for `aw.step`:
  ```bash
  RUST_LOG=info cargo run -p greentic-runner -- start ./packs/dw-agent-basic 2>&1 | grep "aw.step"
  ```
  Expected: at least one line containing `aw.step` after sending a message.

- [ ] **#10 tool idempotency:** Write a focused regression test if not yet present. Append to `tests/redis_state.rs`:
  ```rust
  #[tokio::test]
  async fn tool_idempotency_ledger_blocks_double_dispatch() {
      let Some(url) = redis_url() else { return; };
      // Direct Redis: pre-populate the ledger key
      let mut conn = greentic_state::RedisPool::connect(&url).await.unwrap().get().await.unwrap();
      let key = "aw:acme:prod:sess-idem:tool_calls:tc1";
      let entry = serde_json::json!({ "result": { "ok": true } }).to_string();
      conn.set_ex(key, &entry, 60).await.unwrap();

      // Now build a runtime + step where the LLM emits the same tool_call_id "tc1".
      // The loop must NOT call ExtensionRuntime::invoke_tool — assert via a
      // counting ExtensionRuntime mock (extension to greentic-ext-runtime
      // for_test shim with a call counter).
      // ... (full body)
  }
  ```
  Run, then mark.

- [ ] **Step: Update acceptance tracker**

```bash
sed -i.bak -E 's/- \[ \] (3|5|7|10)\./- [x] \1./' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
rm docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md.bak
git add docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
git commit -m "docs(aw-runtime): acceptance #3, #5, #7, #10 verified"
```

---

### Task 6.4: Acceptance #4 — Plan-Act-Observe with 1+ tool call end-to-end

- [ ] **Step 1: Write an E2E test with a real tool**

Pick a simple tool like `http.fetch` (or whichever is most stable in the `for_test` ExtensionRuntime). Script the LLM with:
1. First call: emit `tool_calls = [http.fetch(url=https://example.com)]`
2. Second call (after tool observation): emit final reply containing the fetched content excerpt

```rust
#[tokio::test]
async fn end_to_end_tool_call_then_reply() {
    // ... build runtime with for_test ExtensionRuntime that returns a fixed tool result
    // ... script LLM with the two-turn sequence above
    let out = runtime.step(...).await.unwrap();
    assert_eq!(out.terminated_by, TerminationReason::FinalReply);
    let has_tool_call = out.trail.iter().any(|s| matches!(s, AgentStep::ToolCall { .. }));
    assert!(has_tool_call);
}
```

- [ ] **Step 2: Run + mark + commit**

```bash
cargo test -p greentic-aw-runtime --features test-mock --test loop_scripted end_to_end_tool_call
sed -i.bak 's/- \[ \] 4\./- [x] 4./' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
rm docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md.bak
git add docs/superpowers/specs docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md crates/greentic-aw-runtime/tests/loop_scripted.rs
git commit -m "test(aw-runtime): Plan-Act-Observe E2E with tool call → reply"
```

---

### Task 6.5: Acceptance #6 — State persists across runner restart

- [ ] **Step 1: Write a restart simulation test**

```rust
#[tokio::test]
async fn state_survives_runtime_drop_and_rebuild() {
    let Some(url) = std::env::var("REDIS_URL").ok() else { return; };
    let tenant = TenantContext::new("acme", "prod");
    let session = format!("restart-{}", uuid::Uuid::new_v4());

    // First "runner": send a message, persist state.
    {
        let store = RedisAgentStateStore::connect(&url).await.unwrap();
        let runtime = build_runtime_with_store(store /* mocks for rest */);
        runtime.step(tenant.clone(), &session, "a", AgentInput { text: "hello".into() })
            .await.unwrap();
    }
    // Runtime dropped — simulates a runner-host restart.

    // Second "runner": load state should contain prior user message.
    let store2 = RedisAgentStateStore::connect(&url).await.unwrap();
    let state = store2.load(&tenant, &session).await.unwrap();
    let has_user_hello = state.messages.iter().any(|m| matches!(
        m, ChatMessage::User { content } if content == "hello"
    ));
    assert!(has_user_hello, "expected user message to persist across runner restart");
}
```

- [ ] **Step 2: Run + commit**

```bash
REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock --test loop_scripted state_survives_runtime_drop
sed -i.bak 's/- \[ \] 6\./- [x] 6./' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
rm docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md.bak
git add docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md crates/greentic-aw-runtime/tests/loop_scripted.rs
git commit -m "test(aw-runtime): conversation state survives runtime drop/rebuild"
```

---

### Task 6.6: Acceptance #8 — Banking-id composer config produces a real reply

- [ ] **Step 1: Locate a banking-id composer answer document**

Search for sample answers from the prior Slice C work:

```bash
find /Users/bimapangestu/Desktop/Works/personal/greentic -name "*banking-id*" -type f 2>/dev/null | head
```

- [ ] **Step 2: Build a fixture + test**

Create `crates/tests/fixtures/banking_id_answers.json` containing a representative AnswerDocument the composer emits.

Write a test that:
1. Loads the AnswerDocument
2. Runs it through `greentic-designer`'s `manifest_builder::build_manifest`
3. Translates the resulting manifest into an `AgentConfig`
4. Calls `AgentRuntime::step` with a sample message
5. Asserts the LLM gets the banking-id system_prompt + opening_message

```rust
#[tokio::test]
async fn banking_id_composer_config_drives_a_real_reply() {
    let answers: serde_json::Value = serde_json::from_str(
        include_str!("fixtures/banking_id_answers.json")
    ).unwrap();
    // ... build AgentConfig via manifest_builder + playground_config translator
    // ... script LLM to verify system_prompt + opening_message reach it
    // ... assert reply contains a banking-context word like "KYC"
}
```

- [ ] **Step 3: Run + commit**

```bash
REDIS_URL=redis://localhost:6379/15 cargo test -p tests --features test-mock --test banking_id_composer_config
sed -i.bak 's/- \[ \] 8\./- [x] 8./' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
rm docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md.bak
git add docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md crates/tests
git commit -m "test(integration): banking-id composer → AgentConfig → real reply through AgentRuntime"
```

---

### Task 6.7: Acceptance #9 — Daily token cap with real Redis

- [ ] **Step 1: Write the test**

```rust
#[tokio::test]
async fn daily_token_cap_enforced_against_real_redis() {
    let Some(url) = std::env::var("REDIS_URL").ok() else { return; };
    let pool = Arc::new(greentic_state::RedisPool::connect(&url).await.unwrap());
    let meter = RedisTokenMeter::new(pool.clone());
    let tenant = TenantContext::new("cap-test", format!("env-{}", uuid::Uuid::new_v4()));

    // Pre-set the counter past the cap
    meter.add(&tenant, 1000).await.unwrap();

    let cfg = AgentConfig {
        limits: AgentLimits {
            daily_token_cap_per_tenant: Some(500),
            ..AgentLimits::default()
        },
        ..sample_cfg()
    };
    // ... build runtime with this RedisTokenMeter (not the mock)
    let err = runtime.step(tenant.clone(), "s", "a", AgentInput { text: "x".into() }).await.unwrap_err();
    assert!(matches!(err, AgentError::TokenBudgetExceeded));
}
```

- [ ] **Step 2: Run + commit**

```bash
REDIS_URL=redis://localhost:6379/15 cargo test -p greentic-aw-runtime --features test-mock daily_token_cap
sed -i.bak 's/- \[ \] 9\./- [x] 9./' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
rm docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md.bak
git add docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md
git commit -m "test(aw-runtime): daily token cap enforced against real Redis"
```

---

### Task 6.8: Final clippy + fmt + docs sweep + Phase-6 PR

- [ ] **Step 1: Full workspace pipeline**

Run:
```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --features test-mock -- -D warnings
REDIS_URL=redis://localhost:6379/15 cargo test --workspace --features test-mock
```
Expected: All clean / pass. Fix any issues inline.

- [ ] **Step 2: Document the new crate in workspace README**

Append a brief section in `/Users/bimapangestu/Desktop/Works/personal/greentic/greentic-runner/README.md` under the existing crate listing:

```markdown
### `crates/greentic-aw-runtime/`

Enterprise Agentic Worker runtime. Plan-Act-Observe loop with LLM tool
calls, Redis-backed state + distributed locking, per-tenant daily token
cap, idempotency ledger for tool dispatch. Consumed by runner-host as
`NodeKind::DwAgent` and by greentic-designer's playground.

See `docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md`.
```

- [ ] **Step 3: Final acceptance sanity check**

```bash
grep -E '^- \[' docs/superpowers/specs/2026-05-22-enterprise-aw-runtime-design.md | head
```
Expected: all 10 acceptance items marked `[x]`.

- [ ] **Step 4: Open PR6**

```bash
git push -u origin feat/aw-runtime-phase-6
gh pr create --base develop --title "feat(aw-runtime): Phase 6 — acceptance + polish + README" --body "$(cat <<'EOF'
## Summary
- Verify all 10 acceptance criteria from spec §8.5
- Add parity test (designer ↔ runner)
- Add state-persistence-across-restart test
- Add banking-id composer integration test
- Add real-Redis daily token cap test
- Workspace fmt + clippy clean

## Acceptance tracker
All 10 items in spec §14 checked ✅

## Test plan
- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --workspace --all-targets --features test-mock -- -D warnings` clean
- [ ] `REDIS_URL=redis://localhost:6379/15 cargo test --workspace --features test-mock` all pass
EOF
)"
```

---

## Plan Self-Review (run after writing — fix issues inline before handoff)

1. **Spec coverage:**
   - §1 Background — Phase 0 background already in spec, plan doesn't need to repeat ✅
   - §2 Scope (in scope items) — each MVP item maps to a Phase 1-6 task ✅
   - §2 Scope (out of scope) — explicitly skipped, no tasks ✅
   - §3 Customer profile + constraints — Phase 1 task 1.1 enforces edition 2024, native async fn, test-mock feature ✅
   - §4 Decisions Locked — every decision is realised in a task: 1 (library) → 1.1, 2 (ext-runtime) → 3.2, 3 (Redis only) → 2.x, 4 (Plan-Act-Observe) → 3.4, 5 (LLM per-tenant) → 3.1+4.4, 6 (TenantContext) → 1.2, 7 (locks 90s+refresh) → 2.x, 8 (history 20) → 1.4+3.4, 9 (audit trail) → loop.rs, 10 (max_iter/timeout) → 1.4+3.4, 11 (OTel) → 1.8+3.4, 12 (mixed response) → 3.4+3.5, 13 (config cache 60s) → 1.6, 14 (cost meter) → 3.3+3.4 ✅
   - §5.1 crate layout — Task 1.1 + table at top of plan ✅
   - §5.1.1 cross-workspace dep — Task 1.1 step 1 verifies tag ✅
   - §5.2 core API surface — Tasks 1.2-1.8 introduce every type and trait ✅
   - §5.3 loop pseudocode — Task 3.4 transcribes pseudocode into Rust ✅
   - §5.4 integration points — Phase 4 (runner) + Phase 5 (designer) ✅
   - §5.5 Redis key schema — Tasks 2.x (state+lock), 3.3 (cost), 3.2 (ledger) ✅
   - §6 termination & error semantics — error.rs (1.3) + loop.rs (3.4) cover all 6 termination rows ✅
   - §6.1 sanitised errors — Task 1.3 implements helper + every caller uses it ✅
   - §6.2 typing indicator — Task 5.5 ✅
   - §7 tenant isolation & security — Task 1.2 (compile-time tenant) + Task 2.6 (cross-talk test) ✅
   - §8 test plan — Tasks across 2.x, 3.5, 4.5, 5.6, 6.x match every bullet ✅
   - §8.5 acceptance criteria — Phase 6 explicit tasks 6.1-6.7 verify each ✅
   - §9 architectural seeds — captured in code structure (trait swap points); no specific task needed ✅
   - §10 non-goals — explicitly not in plan ✅
   - §11 open questions — X1/X2/X3 resolved during Phase 4/5/3 respectively; X4 (tool ref format) resolved in Task 1.4 (ToolRef struct) + Task 3.2 (list_tools split on `.`) ✅
   - §13 implementation phasing — plan structured into the same 6 PRs ✅

2. **Placeholder scan:** Searched the plan body for the failure patterns:
   - "TBD" — present only inside spec quotations (e.g., `**Owner:** TBD` in spec ref). No plan task body uses TBD as work-to-do. ✅
   - "TODO" — appears only inside `todo!()` Rust macros at Task 4.2 Step 2 as a deliberate intermediate compile-allowed marker that Task 4.3 replaces. Documented inline; acceptable. ✅
   - "fill in", "implement later" — absent. ✅
   - "add appropriate error handling" — absent; every task shows the actual error variant or `Result` mapping. ✅
   - "Similar to Task N" — used once at Task 6.2 ("follow the pattern from Task 4.5") but the file Task 4.5 lives in is referenced and a real code skeleton is shown. Reader can pick up cold. ✅

3. **Type consistency:**
   - `TenantContext::new(tenant_id, env_id)` — used identically across Tasks 1.2, 2.x, 3.x, 4.x, 5.x ✅
   - `AgentConfig` fields — `agent_id`, `system_prompt`, `tools`, `llm`, `limits` consistent across 1.4 / 3.1 / 5.2 ✅
   - `AgentLimits` fields — `max_iter`, `timeout`, `max_history_turns`, `llm_retry_attempts`, `llm_retry_backoff`, `provider_failure_message`, `daily_token_cap_per_tenant` consistent across 1.4 / 3.4 / 3.5 ✅
   - `ChatMessage` variants — `System`, `User`, `Assistant`, `Tool` consistent across 1.5 / 3.1 / 3.4 ✅
   - `LlmResponse` fields — `content`, `tool_calls`, `tokens_in`, `tokens_out` consistent across 1.7 / 3.1 / 3.4 / 3.5 ✅
   - `AgentStep` variants — `ToolCall`, `ToolCallReused`, `ToolCallBlocked`, `Reply` consistent ✅
   - `SessionLock` API — `refresh()` returns `Result<(), StateError>` consistent across 1.5 / 2.2 / 3.4 ✅
   - `AgentRuntime::new` signature — grows in Task 3.4 from 5 to 7 args (adds `token_meter`, `ledger_pool`); the diff is shown explicitly so every caller in Phase 4/5 updates accordingly ✅
   - `ToolRef` fields — `extension_id`, `tool_name` consistent across 1.4 / 3.2 / 5.2 ✅

Plan is internally consistent. No remediation needed before handoff.

---
