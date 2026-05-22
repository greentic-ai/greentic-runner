# Enterprise Agentic Worker Runtime — Design Spec

**Status:** Draft — Review Feedback Applied (2026-05-23)
**Date:** 2026-05-22
**Owner:** TBD
**Related:** [`greentic-designer/docs/superpowers/specs/2026-05-19-designer-branding-sync-design.md`](../../../../greentic-designer/docs/superpowers/specs/2026-05-19-designer-branding-sync-design.md) (Slice C, composer integration)

## 1. Background

Greentic positions itself as the "launch lab for AI digital workers" with 10 verticals (dental, legal, real estate, financial advice, healthcare, etc.). The **Agentic Worker (AW)** concept exists today as a design-time abstraction — composer extensions (banking-id, cs-bot, etc.) generate `AnswerDocument` + manifest specs — but **no runtime executes that spec into a working agent**.

DWBase (`greentic-dwbase`) provides memory infrastructure (immutable atom store, vector search) but does not implement an LLM-call-and-tool-dispatch loop. The flow runtime (`FlowEngine` in `greentic-runner-host`) executes 8 NodeKinds against WASM components but has no node type that runs an agentic loop. The canvas extension `dw-canvas-default` ships `dw-orchestrator` / `dw-specialist` node types as palette stubs with explicit comment "runtime wiring lands when DW + Flow runtime extraction completes (Phase D backlog)."

This spec defines that Phase D runtime — the **Agentic Worker Runtime** — as a library crate that both production runner-host and designer test playground consume.

## 2. Scope

**In scope (MVP):**
- Plan-Act-Observe agent loop driven by LLM tool-call decisions
- Tool dispatch via existing `greentic-ext-runtime` WASM extension components
- Conversation state persistence via Redis (reuse `greentic-state` Redis cluster)
- Multi-tenant isolation (logical, key-prefix based)
- LLM provider config-driven from admin designer per tenant
- Integration as `FlowEngine::NodeKind::DwAgent` (production)
- Integration as designer Test playground (preview / dev UX)
- OpenTelemetry traces per step with `terminated_by`, `iterations`, `total_tokens` attributes
- Lightweight audit trail surfaced in `AgentOutput.trail`
- Cost meter enforcement via Redis daily token counter per tenant (moved from Out of Scope — see §S2 below)
- Typing indicator / heartbeat minimum for designer playground UI (step in progress shows "thinking...")

**Out of scope (post-MVP, deferred but architectural seeds preserved):**
- Reflection loop ("did my action achieve the goal?")
- Multi-agent collaboration
- Streaming SSE responses (full reply only; see §6 for heartbeat workaround)
- Cooperative cancel mid-loop
- DwBase atom store as state backend (`AgentStateStore` trait swap)
- Bank-grade tamper-evident audit (signed step hash chain)
- Cross-region sharding / data residency policy
- BYOK / multi-provider routing at LLM layer
- Per-call OTel spans for individual LLM calls and tool calls (deferred post-MVP; MVP keeps only `aw.step` span)
- Pub/sub config cache invalidation (TTL-only for MVP; post-MVP follow-up)
- Conversation summarisation / RAG over long history
- Runner-side streaming (depends on consuming flow node + channel implementation)

## 3. Customer Profile & Constraints

**Target customer (per Greentic Labs positioning):** mid-market and SMB service businesses across 10 verticals. Customer-facing AI for intake, qualification, booking. SaaS-first deployment. Mix of regulated (banking, healthcare, legal) and unregulated (e-commerce, trades) — design accommodates all without making MVP regulator-grade.

**Scale envelope (per user "Optimize for path"):** start small (<100 tenants, low hundreds concurrent conversations) with architectural decisions that support growth-phase (1000s of tenants, 1000s concurrent) without rewrite.

**Constraints:**
- Rust 1.95.0 pinned via workspace `rust-toolchain.toml`
- Edition 2024
- Max 500 lines per Rust source file (workspace rule)
- No `unwrap()` / `panic!()` in production paths
- Async + `Send + Sync` everywhere
- No `#[async_trait]` / `async-trait` crate — use native `async fn` in trait (Edition 2024 RPITIT) throughout
- Conventional Commits, no Claude / AI co-author trailer
- Pre-commit hook (fmt + clippy) and pre-push hook (full pipeline) must pass
- ALL CI test runs use `--features test-mock`; real LLM hits only in smoke tests gated by `GREENTIC_LLM_TEST_BUDGET_USD` env var (CI sets this to 0 by default)

## 4. Decisions Locked

| # | Topic | Choice | Rationale |
|---|---|---|---|
| 1 | Library vs. standalone runtime | Library crate `greentic-aw-runtime`, dual-consumed by runner-host (production) and designer (test playground). | User explicit no over-engineering. Library with async traits supports future RPC extraction without rewrite. Designer playground using same library guarantees production parity. Note: designer's lockfile-pinned `greentic-ext-runtime` version MUST be ≥ runner-host's pinned version to avoid schema-newer-than-supported `ConversationState` errors (see §5.2 `schema_version` note). |
| 2 | Tool source | Existing WASM extension components via `ExtensionRuntime::invoke_tool`. | Reuse existing dispatch infrastructure. Zero new tool runtime. Tools are extension-versioned + sandboxed already. `invoke_tool` is a blocking `fn`; all call sites wrap in `tokio::task::spawn_blocking` (see §5.3). |
| 3 | State backend (MVP) | Redis-only via new `aw:*` namespace; `AgentStateStore` trait abstraction so DwBase can plug in later. | Reuse existing greentic-state Redis cluster, no new ops surface. DwBase adoption deferred (it's the right tool but more setup than MVP needs). |
| 4 | Agent loop semantics | Plan-Act-Observe with LLM tool calls. Termination = LLM-emitted final text reply OR max_iter OR timeout. | Matches "agentic worker" product framing. Tools come from existing WASM extensions, so dispatch path already exists. |
| 5 | LLM provider resolution | Per-tenant config from admin designer; AW runtime receives resolved `LlmProviderRef` and dispatches to provider-agnostic `LlmBackend` trait. | User explicit "config-driven from admin designer". Keeps AW runtime free of billing/provider-routing concerns. |
| 6 | Multi-tenant isolation | Logical via mandatory `TenantContext` parameter on every method + Redis key prefix. Compile-time enforcement (no method accepts state access without tenant). | Cheap, sufficient for MVP, physical isolation (per-tenant cluster) layers on later by smarter trait impl without call-site changes. |
| 7 | Concurrency / locking | Redis `SET NX` distributed mutex per `(tenant, session)` for duration of `step()`. Default wait 5s, lock TTL **90s** (longer than max step timeout of 60s to prevent race). `AgentRuntime::step` calls `lock.refresh()` once per loop iteration to extend the TTL by another 90s window. | Multi-instance runner safety. Avoid race when same session receives concurrent messages. Lock TTL exceeds step timeout so a legitimately slow step cannot lose its lock mid-execution. |
| 8 | History bounds | Default last 20 messages, configurable per `AgentLimits`. Truncation drops oldest user-assistant pairs first; system prompt + opening message preserved. | Simple bound for MVP, sufficient for most chat scenarios. Future RAG/summarisation hook via tool call. |
| 9 | Audit trail | Lightweight: `AgentOutput.trail` lists `AgentStep` per loop iteration. Caller decides storage (runner → telemetry events; designer → UI panel). | MVP "show reasoning" UX. Tamper-evident signing reserved for regulated-vertical follow-up. |
| 10 | Termination defaults | `max_iter: 8`, `timeout: 60s`, configurable per-agent in `AgentLimits`. | Prevents runaway loops without surprising operator. |
| 11 | Observability | Existing `greentic-telemetry` OpenTelemetry integration. MVP span: `aw.step` with attributes `terminated_by`, `iterations`, `total_tokens`, per-tenant attrs. Per-LLM-call and per-tool-call spans deferred to post-MVP (over-engineered for initial release). | Reuse, no new infra. Lean telemetry surface keeps noise low and implementation fast. |
| 12 | Mixed LLM response (text + tool_calls) | When LLM returns both `content` AND `tool_calls` in one response, execute `tool_calls` and discard the accompanying `content`. The text content in such mixed responses is a reasoning trace, not a final user reply; presence of tool calls means the agent has not yet decided to terminate. | Deterministic, avoids ambiguity. |
| 13 | Config cache invalidation | TTL-only (60s) for MVP. Expect up to 60s propagation lag in multi-instance runners. Pub/sub invalidation is a post-MVP follow-up. | Simple, no extra infra; acceptable lag for operator config changes. |
| 14 | Cost meter | Redis daily token counter per tenant enforced at `step()` entry; returns `AgentError::TokenBudgetExceeded` with sanitised fallback message when cap is hit. | Prevents runaway billing; lightweight enforcement that reuses existing Redis cluster. |

## 5. Architecture

### 5.1 Crate layout

```
greentic-runner/                      (existing workspace)
└── crates/
    ├── greentic-runner-host/         (existing — FlowEngine lives here)
    └── greentic-aw-runtime/          NEW
        ├── src/
        │   ├── lib.rs                Re-exports + AgentRuntime entry point
        │   ├── loop.rs               Plan-Act-Observe loop core
        │   ├── config.rs             AgentConfig, AgentLimits, LlmProviderRef
        │   ├── state.rs              ConversationState + AgentStateStore trait + SessionLock
        │   ├── state_redis.rs        RedisAgentStateStore (default impl) + RedisSessionLock
        │   ├── tools.rs              Tool resolution + dispatch (wraps invoke_tool in spawn_blocking)
        │   ├── llm.rs                LlmBackend trait + OpenAI default impl + RetryingLlmBackend decorator
        │   ├── tenant.rs             TenantContext
        │   ├── telemetry.rs          OTel span emission helpers (aw.step span only)
        │   └── error.rs              AgentError, TerminationReason
        └── tests/                    Unit (mocks, --features test-mock) + integration (real Redis)
```

`greentic-aw-runtime` depends on:
- `greentic-ext-runtime` (tool dispatch — see §5.1.1 for cross-workspace wiring)
- `greentic-state` for Redis pool reuse
- `greentic-telemetry` for OTel
- Standard async libs (tokio, serde, serde_json, anyhow, thiserror)
- Does NOT depend on `async-trait` crate (Edition 2024 native async fn in trait)

Does NOT depend on:
- `greentic-runner-host` (consumed by it, not consumer of it)
- `greentic-flow` (no flow model leakage)
- `greentic-designer` (designer pulls aw-runtime as workspace-external git dep)

#### 5.1.1 Cross-workspace dependency on `greentic-ext-runtime`

`greentic-ext-runtime` lives in the `greentic-designer-extensions` workspace with `publish = false`. It cannot be consumed via crates.io. `greentic-aw-runtime/Cargo.toml` therefore uses a git dependency:

```toml
[dependencies]
greentic-ext-runtime = { git = "https://github.com/greentic-biz/greentic-designer-extensions", tag = "v1.2.8-research" }
```

The tag `v1.2.8-research` is the latest tag at time of writing; verify the correct tag before implementation begins.

**Version-bump coordination requirement:** the tag pinned in `greentic-aw-runtime/Cargo.toml` and the tag already pinned by `greentic-runner-host` (which also consumes `greentic-ext-runtime`) MUST match. Mismatched versions produce trait incompatibilities across the workspace boundary (Rust orphan rules + different type definitions in scope). Any `greentic-ext-runtime` upgrade must be coordinated as a single PR that bumps both `greentic-aw-runtime` and `greentic-runner-host` to the same new tag simultaneously.

### 5.2 Core API surface

Note: all traits use native `async fn` in trait (Edition 2024 RPITIT). No `#[async_trait]` macro or `async-trait` crate — this matches the workspace "no async-trait crate" convention established in the admin codebase.

```rust
pub struct AgentRuntime {
    config_provider: Arc<dyn ConfigProvider>,
    state_store:     Arc<dyn AgentStateStore>,
    ext_runtime:     Arc<ExtensionRuntime>,
    llm:             Arc<dyn LlmBackend>,
    telemetry:       Arc<dyn Telemetry>,
}

impl AgentRuntime {
    pub async fn step(
        &self,
        tenant:     TenantContext,
        session_id: &str,
        agent_id:   &str,
        message:    AgentInput,
    ) -> Result<AgentOutput, AgentError>;
}

/// Provides agent configuration per tenant and agent ID.
/// Implementations MUST cache responses for up to 60s (config TTL).
pub trait ConfigProvider: Send + Sync {
    async fn agent_config(
        &self,
        tenant: &TenantContext,
        agent_id: &str,
    ) -> Result<AgentConfig, ConfigError>;
}

/// Persists and locks conversation state.
/// Implementations are responsible for atomic load-save semantics.
pub trait AgentStateStore: Send + Sync {
    /// Load conversation state. If no state exists, return an empty initialised state.
    async fn load(
        &self,
        tenant: &TenantContext,
        session_id: &str,
    ) -> Result<ConversationState, StateError>;

    /// Persist conversation state.
    async fn save(
        &self,
        tenant: &TenantContext,
        session_id: &str,
        state: &ConversationState,
    ) -> Result<(), StateError>;

    /// Acquire a distributed lock for the given session.
    ///
    /// Blocks for up to `wait` duration until the lock is acquired.
    /// Returns an RAII [`SessionLock`] whose `Drop` releases the lock.
    /// Lock TTL is 90s; callers MUST call `lock.refresh()` periodically
    /// (once per loop iteration) to extend the hold duration.
    ///
    /// # Errors
    /// Returns [`StateError::LockTimeout`] if the lock cannot be acquired
    /// within `wait`.
    async fn acquire_lock(
        &self,
        tenant: &TenantContext,
        session_id: &str,
        wait: Duration,
    ) -> Result<SessionLock, StateError>;
}

/// RAII distributed-lock guard returned by [`AgentStateStore::acquire_lock`].
///
/// Dropping this value releases the lock (best-effort; relies on Redis
/// SET NX TTL as the safety net if the process dies before Drop runs).
pub struct SessionLock {
    // Implementation-detail fields; not part of public API.
    // RedisSessionLock holds the Redis key + connection handle.
    _inner: Box<dyn SessionLockInner>,
}

impl SessionLock {
    /// Refresh the lock TTL, extending the hold duration by another 90s window.
    ///
    /// The runtime loop calls this once per iteration so a long-running
    /// `step()` does not lose its lock mid-execution.
    ///
    /// # Errors
    /// Returns [`StateError`] on Redis failure; the caller (loop) should
    /// log the error and continue — losing the TTL extension is preferable
    /// to aborting a partially-complete agent turn.
    pub async fn refresh(&self) -> Result<(), StateError>;
}

// Drop impl releases the lock.
// Note: Drop cannot be async; the implementation uses a background task
// or a blocking Redis call to perform the key deletion on drop.

/// Sealed trait for lock inner implementations.
trait SessionLockInner: Send + Sync {
    fn refresh_blocking(&self) -> Result<(), StateError>;
    fn release(&self);
}

/// Calls the LLM provider. Implementations wrap retry policy via
/// [`RetryingLlmBackend`].
pub trait LlmBackend: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;
}

/// Decorator that wraps any [`LlmBackend`] with the retry policy from
/// [`AgentLimits`]. This is the default backend used by [`AgentRuntime`].
pub struct RetryingLlmBackend<B: LlmBackend> {
    inner:    B,
    attempts: u32,
    backoff:  Duration, // initial; exponential
}

impl<B: LlmBackend + Send + Sync> LlmBackend for RetryingLlmBackend<B> {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;
    // Retries on LlmError::ServiceUnavailable; does not retry on 4xx-class errors.
}

/// Records observability signals for the agent step.
pub trait Telemetry: Send + Sync {
    /// Record a completed step. This is the ONLY OTel span emitted per step (MVP).
    /// Per-LLM-call and per-tool-call spans are deferred to post-MVP.
    fn record_step(&self, ctx: &StepTelemetryCtx);
}

/// Context for the single `aw.step` OTel span.
pub struct StepTelemetryCtx {
    pub tenant_id:    String,
    pub env_id:       String,
    pub session_id:   String,
    pub agent_id:     String,
    pub terminated_by: TerminationReason,
    pub iterations:   u32,
    pub total_tokens: u64,
    pub duration:     Duration,
}
```

#### AgentConfig and AgentLimits

```rust
pub struct AgentConfig {
    pub agent_id:     String,
    pub system_prompt: String,
    pub tools:        Vec<ToolRef>,      // allowed tool set for this agent
    pub llm:          LlmProviderRef,    // which LLM provider + model
    pub limits:       AgentLimits,
}

pub struct AgentLimits {
    /// Maximum number of Plan-Act-Observe iterations per step. Default: 8.
    pub max_iter: u32,
    /// Wall-clock timeout for the entire step() call. Default: 60s.
    pub timeout: Duration,
    /// Maximum retained conversation history turns. Default: 20.
    /// Truncation drops oldest user-assistant pairs; system prompt preserved.
    pub max_history_turns: u32,
    /// LLM call retry attempts before declaring provider unavailable. Default: 3.
    pub llm_retry_attempts: u32,
    /// Initial backoff for LLM retries; exponential. Default: 250ms.
    pub llm_retry_backoff: Duration,
    /// Tenant-configurable user-facing message when LLM provider is unavailable
    /// after all retries. Defaults to:
    /// "I'm having trouble reaching my reasoning system. Please try again in a moment."
    pub provider_failure_message: Option<String>,
    /// Daily token cap per tenant. When set and exceeded, step() returns
    /// AgentError::TokenBudgetExceeded. Default: None (uncapped).
    pub daily_token_cap_per_tenant: Option<u32>,
}
```

#### ConversationState

```rust
pub struct ConversationState {
    /// Schema version. MUST be the first field to allow forward-compatibility checks.
    /// Current supported version: 1.
    ///
    /// On AgentStateStore::load:
    ///   - If schema_version > supported → return StateError::SchemaIncompatible.
    ///   - If schema_version < supported → attempt migration (MVP: only version 1
    ///     supported; future versions add migrate() paths).
    pub schema_version: u32,
    pub session_id:     String,
    pub tenant_id:      String,
    pub env_id:         String,
    pub messages:       Vec<ChatMessage>,
    pub created_at:     DateTime<Utc>,
    pub updated_at:     DateTime<Utc>,
}
```

### 5.3 Plan-Act-Observe loop

**Key implementation notes before the pseudocode:**

1. **`invoke_tool` is blocking.** `ExtensionRuntime::invoke_tool` is a synchronous `fn`, not `async fn`. It performs Wasmtime WASM dispatch, which is CPU-bound and may block the executor thread for up to several seconds. Every call site MUST wrap it in `tokio::task::spawn_blocking`:
   ```rust
   let result = tokio::task::spawn_blocking(move || {
       ext_runtime.invoke_tool(ext_id, tool_name, &args_json)
   })
   .await??;
   ```
   This is enforced in `tools.rs` — no other module calls `invoke_tool` directly.

2. **Idempotency ledger for tool calls.** Each tool call is recorded in Redis before dispatch using its `tool_call_id`. If state save fails after a tool was successfully dispatched, the next `step()` replay will find the call_id in the ledger and skip re-dispatch, using the stored result. This prevents duplicate side effects (e.g., sending the same email twice).

3. **Lock refresh per iteration.** The loop calls `lock.refresh()` once per iteration to extend the 90s TTL. If refresh fails, log a warning and continue — losing the TTL extension is preferable to aborting a partially-complete turn.

4. **Mixed text + tool_calls response.** When the LLM returns both `content` AND `tool_calls` in one response, the `tool_calls` take precedence. The `content` is discarded (treated as a reasoning trace, not a final reply). The loop continues to the next iteration.

Pseudocode:

```
step(tenant, session_id, agent_id, message):
  // --- Cost budget check ---
  token_key = "aw:{tenant}:{env}:cost:tokens:{today_yyyymmdd}"
  daily_tokens = redis.get(token_key) or 0
  if config.limits.daily_token_cap_per_tenant is Some(cap) and daily_tokens >= cap:
    return AgentError::TokenBudgetExceeded with sanitised fallback reply

  // --- Acquire distributed lock (default wait: 5s) ---
  lock = state_store.acquire_lock(tenant, session_id, 5s)
  // Lock TTL is 90s; refreshed once per loop iteration below.

  // --- Load config (cached 60s) and state ---
  config = config_provider.agent_config(tenant, agent_id)
  state  = state_store.load(tenant, session_id)        // init empty if not found
  state.messages.push(User(message.text))

  total_tokens = 0
  trail = []
  start_time = Instant::now()

  for iter in 0..config.limits.max_iter:
    // Refresh distributed lock TTL before each iteration.
    lock.refresh()  // log warning on failure; do not abort

    if start_time.elapsed() >= config.limits.timeout:
      break(Timeout)

    tools_schema = ext_runtime.list_tools(config.tools)
    request = LlmRequest {
        system_prompt: config.system_prompt,
        history: state.messages,
        tools: tools_schema,
    }
    // LLM call with retry (via RetryingLlmBackend):
    response = llm.complete(request)
    // On LlmError::ServiceUnavailable after all retries:
    //   state_store.save(tenant, session_id, state)  // best-effort; preserve progress
    //   return AgentError::LlmProviderUnavailable
    //   (caller surfaces provider_failure_message to end-user)

    total_tokens += response.tokens_in + response.tokens_out
    redis.incrby(token_key, response.tokens_in + response.tokens_out)
    redis.expire(token_key, 86400)  // rolling 24h window

    match response:
      // Both text and tool_calls present: tool_calls win (mixed response rule).
      ToolCalls(calls) | MixedResponse(_, calls):
        for call in calls:
          if not allowed(call.name, config.tools):
            state.messages.push(Tool(call_id=call.id, error="not allowed"))
            trail.push(ToolCallBlocked(call.name))
            continue

          // --- Idempotency check ---
          ledger_key = "aw:{tenant}:{env}:{session}:tool_calls:{call.id}"
          if redis.exists(ledger_key):
            cached_result = redis.get(ledger_key)
            state.messages.push(Tool(call_id=call.id, result=cached_result))
            trail.push(ToolCallReused(call.name, call.id))
            continue

          // --- Dispatch (blocking WASM call via spawn_blocking) ---
          result = tokio::task::spawn_blocking(move || {
              ext_runtime.invoke_tool(call.ext_id, call.name, call.args)
          }).await??

          // --- Record in ledger before updating state ---
          redis.set(ledger_key, result, ex=7days)

          state.messages.push(Tool(call_id=call.id, result))
          trail.push(ToolCall(call.name, call.id, result))

        continue loop  // next LLM turn with tool observations

      FinalReply(text):
        state.messages.push(Assistant(text))
        trail.push(Reply(text))
        break(FinalReply)

  if iter >= max_iter: break(MaxIterations)

  truncate(state.messages, config.limits.max_history_turns)
  state_store.save(tenant, session_id, state)  // best-effort; log on failure
  // Note: tool calls already recorded in idempotency ledger will not re-fire
  // on the next step() even if this save fails. Only the LLM re-call happens.

  telemetry.record_step(StepTelemetryCtx {
      terminated_by, iterations: iter, total_tokens, duration: elapsed, ...
  })

  return AgentOutput { reply, trail, terminated_by }
```

### 5.4 Integration points

**Production (runner-host):**
- `FlowEngine` adds `NodeKind::DwAgent { agent_id: String }`
- Dispatch handler calls `aw_runtime.step(...)` with tenant + session from flow context
- AgentConfig source: `.gtpack` metadata (CBOR-encoded AgentConfig spec)
- StateStore: `RedisAgentStateStore` against production Redis cluster

**Designer test playground:**
- `greentic-designer` adds `greentic-aw-runtime` as git dep
- New / extended `/api/chat` route uses same library
- AgentConfig source: `InMemoryConfigProvider` deriving from live `DwFormState`
- StateStore: Redis with TTL prefix, OR in-memory under `test-mock` feature
- Tenant context: `"designer-preview"` + operator id
- Trail surfaced in UI panel for "Show reasoning"
- While `step()` is in progress, the designer playground UI shows a "thinking..." typing indicator (heartbeat). This is the minimum viable UX to avoid blank-screen behaviour; full streaming SSE is post-MVP.

**Session ID contract:**
- Session IDs are **caller-supplied** (not minted by the library).
- Must be unique per `(tenant_id, session_id)` pair (tenant scoping makes uniqueness easier).
- Must be stable across reconnects within the same conversation (WebChat thread, Slack thread, etc.).
- Format: opaque string ≤ 256 characters.
- Runner derives session ID from the provider's session/thread identifier (e.g., WebChat `conversation_id`).
- Designer playground mints a UUID v4 per operator browser session.

### 5.5 Redis key schema

```
aw:{tenant}:{env}:{session}:state             ConversationState (JSON-encoded), TTL 7 days
aw:{tenant}:{env}:{session}:lock              Mutex SET NX, TTL 90s (refreshed per loop iter)
aw:{tenant}:{env}:meta:{agent_id}:config      Cached AgentConfig, TTL 60s
aw:{tenant}:{env}:audit:{session}:{step_id}   Audit step (optional, feature flag)
aw:{tenant}:{env}:cost:tokens:{yyyymmdd}      Daily token counter (INCRBY), TTL 86400s (24h)
aw:{tenant}:{env}:{session}:tool_calls:{id}   Tool call idempotency ledger entry, TTL 7 days
```

Distinct from `greentic-state` flow-state namespace; no collision.

**Config cache invalidation:** TTL-only (60s) for MVP. Operators should expect up to 60s lag for config propagation in multi-instance runner deployments. Pub/sub invalidation is a post-MVP follow-up tracked in §11.

**Schema version handling:** On `AgentStateStore::load`, if `ConversationState.schema_version` > the supported version (currently `1`), return `StateError::SchemaIncompatible`. If lower, attempt migration (MVP: only version `1` supported; future versions add `migrate()` paths in `state_redis.rs`). Designer's lockfile-pinned `greentic-aw-runtime` version must be ≥ runner-host's to avoid writing a schema version that the runner cannot read (see Decision 1 note in §4).

## 6. Termination & Error Semantics

| Termination | Cause | `AgentOutput.terminated_by` | User-facing reply |
|---|---|---|---|
| `FinalReply` | LLM emits text without tool calls | OK path | LLM reply verbatim |
| `MaxIterations` | Loop count ≥ `config.limits.max_iter` (default 8) | `MaxIterations` | Last partial reply, or configurable stub |
| `Timeout` | Wall-clock ≥ `config.limits.timeout` (default 60s) | `Timeout` | Timeout-aware stub message |
| `Error(reason)` | LLM provider unavailable after retries, state load failure, unhandled exception | `Error` | Sanitised via `AgentError::user_facing_message()` |
| `TokenBudgetExceeded` | Daily token cap hit | `TokenBudgetExceeded` | Configurable budget-exceeded message |
| `Cancelled` | (Post-MVP) external cancel | n/a MVP | n/a |

**LLM provider unavailability:** When the primary provider returns 5xx after all retry attempts are exhausted, the agent returns `Error` termination. MVP does NOT perform cross-provider routing / failover (deferred). The user-facing reply is `AgentConfig.limits.provider_failure_message` (default: "I'm having trouble reaching my reasoning system. Please try again in a moment.").

**Tool dispatch errors** are NOT termination — the error becomes an observation message and the agent continues to the next LLM turn.

**State load failure:** Init empty state with warning log; user message preserved. State save failure: return reply but log error; next turn rebuilds from Redis snapshot or starts fresh (acceptable for MVP; rare in practice).

**Idempotency on save failure:** Tool calls already recorded in the idempotency ledger (key `aw:{tenant}:{env}:{session}:tool_calls:{tool_call_id}`) will not re-fire on the next `step()` even if state save fails mid-loop. Only the LLM re-call happens, using the ledger-stored tool results as observations.

**Lock refresh expectation:** The runtime loop calls `lock.refresh()` once per iteration. If the process dies mid-loop, Redis SET NX TTL (90s) is the safety net — the lock expires naturally within 90s and the next `step()` call can acquire it.

### 6.1 Sanitised error messages

```rust
impl AgentError {
    /// Returns a sanitised, end-user-appropriate string.
    /// No Rust error chain, no internal detail, no PII leakage.
    /// Each variant has a tenant-configurable override via AgentConfig.
    ///
    /// # Example
    /// ```
    /// let message = AgentError::LlmProviderUnavailable.user_facing_message(&config);
    /// // → "I'm having trouble reaching my reasoning system. Please try again in a moment."
    /// ```
    pub fn user_facing_message(&self, config: &AgentConfig) -> String;
}
```

All surfaces (runner, designer) MUST use `AgentError::user_facing_message()` when constructing the reply sent to end-users. Raw `Display` of `AgentError` is only for internal logs.

### 6.2 Typing indicator for MVP (SSE deferred)

Full streaming SSE is out of scope for MVP. However, a blank-screen UX while `step()` runs (up to 60s) is not acceptable.

**Minimum viable workaround:**
- **Designer playground:** UI shows "thinking..." indicator while the `/api/chat` HTTP request is in-flight. This requires no change to the `AgentRuntime` API — it is purely a frontend concern in the designer.
- **Runner / WebChat:** Depends on the consuming flow node and channel implementation. This is out of scope for the AW runtime itself; document for the runner-host integration work.

## 7. Tenant Isolation & Security

- `TenantContext` mandatory on every public method. Compile-time enforced.
- Redis keys prefixed with `tenant_id` + `env`. No cross-tenant access path.
- Agent config resolved per-tenant via `ConfigProvider`. Config not shared across tenants.
- Tool dispatch via `ExtensionRuntime::invoke_tool` already tenant-scoped (existing greentic behaviour).
- LLM provider credentials never persisted in `ConversationState` or `AgentConfig` cache; resolved fresh per call via admin config.
- Daily token budget enforced at `step()` entry to prevent runaway LLM spend (see §5.2 `AgentLimits.daily_token_cap_per_tenant`).
- Future hooks (not enforced MVP):
  - Per-tenant rate limits (record only)
  - Audit signing / hash chain (regulated-vertical post-MVP)

## 8. Test Plan

### 8.1 Unit tests (in-crate `tests` dir or `#[cfg(test)]`)

**All unit tests run under `--features test-mock`. Real LLM hits are strictly prohibited in CI.**

Real LLM calls are only permitted in smoke tests gated by the `GREENTIC_LLM_TEST_BUDGET_USD` environment variable. CI sets this to `0`, which disables smoke tests. Smoke test budget is set manually for release validation runs.

Test cases:
- `step` happy path: user → LLM emits text → reply
- `step` with one tool call: LLM emits tool call → tool returns → LLM emits final reply → reply
- `step` with mixed response (text + tool_calls): tool_calls execute, text content discarded, loop continues
- `step` max_iter termination: LLM keeps emitting tool calls indefinitely, terminate at 8
- `step` timeout: mock LLM hangs, terminate at 60s
- `step` tool not allowed: LLM calls tool not in config, agent observes error, retries, eventually replies
- `step` LLM 5xx with retry: backend errors twice, succeeds third → completes
- `step` LLM 5xx after all retries: returns `AgentError::LlmProviderUnavailable`; reply = `provider_failure_message`
- `step` state load failure: store returns error, step proceeds with empty state, save still attempted
- `step` truncation: history > max_turns triggers oldest-pair drop, system prompt preserved
- `step` token budget exceeded: daily cap hit at step entry, returns `TokenBudgetExceeded`
- `step` tool idempotency: tool call succeeds, state save fails, second `step()` reuses ledger result without re-dispatching
- `step` spawn_blocking wrapping: mock verifies tool dispatch never blocks the async executor
- `TenantContext` propagation: every internal call carries tenant; mocks assert
- `AgentError::user_facing_message`: each variant returns non-empty sanitised string; no internal detail leaked
- `SessionLock::refresh`: refresh extends TTL; refresh failure does not abort loop

### 8.2 Integration tests (`tests/`)

- Real Redis: multi-turn conversation persists across `step()` calls
- Redis: concurrent `step()` on same session blocked by lock
- Redis: lock TTL refresh keeps lock alive across multiple iterations
- Redis: lock expires (simulated) → next `step()` re-acquires successfully
- Redis: TTL refresh on save
- Multi-tenant: two tenants in same Redis, verify zero cross-talk
- Real `ExtensionRuntime` (test harness via `spawn_blocking`): tool call dispatched + observation returned
- Schema version mismatch: `schema_version > 1` in Redis → `StateError::SchemaIncompatible`
- Tool idempotency ledger: entry exists before dispatch → dispatch skipped, stored result used
- Daily token counter: INCRBY increments correct key per tenant+date

### 8.3 Designer playground end-to-end

- Test mode invokes `/api/chat` → real `AgentRuntime` → reply roundtrip
- `DwFormState` change → next-turn behaviour reflects change
- Trail surfaces in UI panel (assertion: panel populated)
- "thinking..." indicator displayed while request is in flight (frontend assertion)

### 8.4 Runner end-to-end

- Flow definition: `WebChat → DwAgent → reply`, mock LLM via `test-mock` feature or env var
- Verify reply traverses graph, `aw.step` telemetry span emitted, state persists
- Verify `NodeKind::DwAgent` handler wraps session_id from flow context correctly

### 8.5 Acceptance criteria (MVP ship)

1. Flow with `WebChat → DwAgent → reply` deployable + functional
2. Designer test playground produces identical agent behaviour to deployed flow
3. Multi-tenant: 2 tenants in same Redis cluster, zero cross-talk verified
4. Plan-Act-Observe with 1+ tool call observed end-to-end
5. Max iter + timeout enforcement verified
6. State persists across runner restart
7. `aw.step` telemetry span visible per agent step with `terminated_by`, `iterations`, `total_tokens` attributes
8. Banking-id composer's generated config → real reply (composer + AW pipeline integrated)
9. Daily token cap enforcement: budget-exceeded path tested with real Redis
10. Tool idempotency: double-dispatch regression test passes

## 9. Architectural Seeds for Future Scale

Decisions designed so future work doesn't require rewrite:

| Future need | Seed in MVP |
|---|---|
| Extract AW runtime to standalone service | **Honest assessment:** Library API surface is async + `Send + Sync` + serde-able payloads — RPC-friendly. However, `Arc<ExtensionRuntime>` holds Wasmtime in-process state and is not serializable across a process boundary. Realistic extraction = define a `ToolDispatcher` RPC contract parallel to the AW service, and replace `Arc<ExtensionRuntime>` in `AgentRuntime` with `Arc<dyn ToolDispatcher>` with a network-backed impl. This is not a one-line wrap — plan for that refactor when extraction becomes real. |
| DwBase atom backend | `AgentStateStore` trait swap, no library or caller change |
| Multi-provider LLM routing per tenant | `LlmBackend` trait; route inside backend impl |
| Per-tenant cost/budget enforcement | Daily token counter already enforced (MVP). Production-grade: `Telemetry::record_step` already emits `total_tokens` per-tenant; enforcement plugs into telemetry sink for richer per-model breakdown |
| Per-region deployment / data residency | `TenantContext.env` + smarter `ConfigProvider` / `StateStore` impls |
| Streaming SSE responses | `step()` returns `AgentOutput`; add `step_stream()` parallel API later |
| Cooperative cancel mid-loop | Add `CancelToken` parameter; library checks at iteration boundaries |
| Agent reflection / multi-agent | Loop variant in `loop.rs`, new `step_reflect()` API |
| Per-LLM-call + per-tool-call OTel spans | `Telemetry` trait already has `record_step`; add `record_llm_call` + `record_tool_call` variants post-MVP without breaking callers |

## 10. Non-Goals

- **No** new ops infrastructure (uses existing Redis cluster, existing OTel collector)
- **No** changes to existing `FlowEngine` node types (only adds `DwAgent`)
- **No** changes to admin designer config schema beyond what already exists (`LlmProviderRef`)
- **No** breaking changes to composer extension interface (`greentic:dw-composer/composer@0.1.0`)
- **No** new auth/identity model — TenantContext is read from existing flow/designer auth

## 11. Open Questions

The following questions have been resolved for MVP:

- ~~Cached config invalidation: simple TTL OR pub/sub?~~ → **Resolved: TTL (60s) for MVP.** Pub/sub invalidation is a post-MVP follow-up. Expect up to 60s propagation lag in multi-instance runners.
- ~~Redis cluster sharing with `greentic-state`~~ → **Resolved: same instance with namespace separation** (key prefix `aw:`) is acceptable for MVP.
- ~~Error message UX: surface raw `AgentError::Display` OR sanitised?~~ → **Resolved: all surfaces use `AgentError::user_facing_message()`** (see §6.1). Raw `Display` is internal-log-only.

The following questions remain open or require their own work items:

**Cross-spec dependency — requires own brainstorm cycle:**

- **X1: `FlowEngine::NodeKind::DwAgent` wiring in `greentic-runner-host`** — the runner-host integration (session_id derivation, TenantContext plumbing, error surfacing back into flow graph) warrants a separate mini-spec in runner-host, or can be folded into Phase 4 of this spec's implementation plan (see §13). Clarify before Phase 4 begins.

- **X2: Designer `/api/chat` route + `DwFormState → AgentConfig` translation** — the designer-side API contract (`InMemoryConfigProvider`, form field mapping, session lifetime in playground) warrants a separate designer spec, or can be addressed as a mini-section within this spec's implementation plan Phase 5. Clarify before Phase 5 begins.

- **X3: Admin designer LLM provider config UI** — verify that the admin designer already has a functioning LLM provider configuration UI (list, add, edit, test-connection). If the UI is absent or incomplete, raise a separate admin spec before Phase 3 begins. The AW runtime assumes a valid `LlmProviderRef` is resolvable at runtime.

- **X4: `AgentConfig` tool reference format** — should `tools` reference extension IDs by full path or short alias? Punt to plan phase. Verify against how `ExtensionRuntime::list_tools` expects tool refs before implementation.

## 12. References

- Mono-workspace CLAUDE.md: `/Users/bimapangestu/Desktop/Works/personal/greentic/CLAUDE.md`
- `greentic-dwbase` workspace: storage substrate (not executor) — future state backend
- `greentic-ext-runtime`: tool dispatch via `invoke_tool` (blocking `fn`); source at `greentic-designer-extensions/crates/greentic-ext-runtime/src/runtime.rs`; current tag `v1.2.8-research` (`publish = false`)
- `greentic-state` + `greentic-session`: Redis pattern reuse
- `greentic-telemetry`: OTel integration
- `greentic.dw.composer.banking-id-1.3.0-research`: prototype composer that produces `AnswerDocument` consumed by this runtime
- Greentic Labs landing: `https://greentic-labs.lovable.app/` (target customer positioning)

## 13. Implementation Phasing

Even within the MVP, work splits naturally into six phased PRs to keep blast radius manageable and allow independent review at each gate. Total honest estimate: **4–6 engineer-weeks**.

| Phase | Duration | Scope | PR gate |
|---|---|---|---|
| 1 | 1.5 wks | Library crate skeleton + all traits + unit tests with mocks (`--features test-mock`) | Traits frozen; unit tests green; `cargo clippy` clean |
| 2 | 1 wk | Redis state backend (`RedisAgentStateStore`) + `SessionLock` + distributed locking + integration tests | Integration tests green against real Redis |
| 3 | 1 wk | LLM backend (`LlmBackend` impl) + `RetryingLlmBackend` decorator + provider config integration + budget counter | LLM retry + budget tests pass; X3 (admin LLM UI) verified |
| 4 | 0.5 wk | Runner-host `NodeKind::DwAgent` wiring | E2E runner flow test passes; X1 resolved |
| 5 | 1 wk | Designer `/api/chat` route + `DwFormState → AgentConfig` translation + playground UI heartbeat | Designer playground roundtrip test passes; X2 resolved |
| 6 | 1 wk | Acceptance testing, e2e with real extensions, bug fixes, §8.5 checklist sign-off | All acceptance criteria met |

Each phase produces a standalone mergeable PR. Later phases have no compile-time dependency on earlier phases being merged (use git branch deps where needed), but semantic dependencies exist — Phase 4 cannot ship before Phase 1–3 are functionally complete.
