# Enterprise Agentic Worker Runtime — Design Spec

**Status:** Draft
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
- OpenTelemetry traces per step, per LLM call, per tool call
- Lightweight audit trail surfaced in `AgentOutput.trail`

**Out of scope (post-MVP, deferred but architectural seeds preserved):**
- Reflection loop ("did my action achieve the goal?")
- Multi-agent collaboration
- Streaming SSE responses (full reply only)
- Cooperative cancel mid-loop
- DwBase atom store as state backend (`AgentStateStore` trait swap)
- Bank-grade tamper-evident audit (signed step hash chain)
- Cross-region sharding / data residency policy
- BYOK / multi-provider routing at LLM layer
- Cost meter enforcement (record-only MVP)
- Conversation summarisation / RAG over long history

## 3. Customer Profile & Constraints

**Target customer (per Greentic Labs positioning):** mid-market and SMB service businesses across 10 verticals. Customer-facing AI for intake, qualification, booking. SaaS-first deployment. Mix of regulated (banking, healthcare, legal) and unregulated (e-commerce, trades) — design accommodates all without making MVP regulator-grade.

**Scale envelope (per user "Optimize for path"):** start small (<100 tenants, low hundreds concurrent conversations) with architectural decisions that support growth-phase (1000s of tenants, 1000s concurrent) without rewrite.

**Constraints:**
- Rust 1.95.0 pinned via workspace `rust-toolchain.toml`
- Edition 2024
- Max 500 lines per Rust source file (workspace rule)
- No `unwrap()` / `panic!()` in production paths
- Async + `Send + Sync` everywhere
- Conventional Commits, no Claude / AI co-author trailer
- Pre-commit hook (fmt + clippy) and pre-push hook (full pipeline) must pass

## 4. Decisions Locked

| # | Topic | Choice | Rationale |
|---|---|---|---|
| 1 | Library vs. standalone runtime | Library crate `greentic-aw-runtime`, dual-consumed by runner-host (production) and designer (test playground). | User explicit no over-engineering. Library with async traits supports future RPC extraction without rewrite. Designer playground using same library guarantees production parity. |
| 2 | Tool source | Existing WASM extension components via `ExtensionRuntime::invoke_tool`. | Reuse existing dispatch infrastructure. Zero new tool runtime. Tools are extension-versioned + sandboxed already. |
| 3 | State backend (MVP) | Redis-only via new `aw:*` namespace; `AgentStateStore` trait abstraction so DwBase can plug in later. | Reuse existing greentic-state Redis cluster, no new ops surface. DwBase adoption deferred (it's the right tool but more setup than MVP needs). |
| 4 | Agent loop semantics | Plan-Act-Observe with LLM tool calls. Termination = LLM-emitted final text reply OR max_iter OR timeout. | Matches "agentic worker" product framing. Tools come from existing WASM extensions, so dispatch path already exists. |
| 5 | LLM provider resolution | Per-tenant config from admin designer; AW runtime receives resolved `LlmProviderRef` and dispatches to provider-agnostic `LlmBackend` trait. | User explicit "config-driven from admin designer". Keeps AW runtime free of billing/provider-routing concerns. |
| 6 | Multi-tenant isolation | Logical via mandatory `TenantContext` parameter on every method + Redis key prefix. Compile-time enforcement (no method accepts state access without tenant). | Cheap, sufficient for MVP, physical isolation (per-tenant cluster) layers on later by smarter trait impl without call-site changes. |
| 7 | Concurrency / locking | Redis `SET NX` distributed mutex per `(tenant, session)` for duration of `step()`. Default wait 5s, lock TTL 30s. | Multi-instance runner safety. Avoid race when same session receives concurrent messages. |
| 8 | History bounds | Default last 20 messages, configurable per `AgentLimits`. Truncation drops oldest user-assistant pairs first; system prompt + opening message preserved. | Simple bound for MVP, sufficient for most chat scenarios. Future RAG/summarisation hook via tool call. |
| 9 | Audit trail | Lightweight: `AgentOutput.trail` lists `AgentStep` per loop iteration. Caller decides storage (runner → telemetry events; designer → UI panel). | MVP "show reasoning" UX. Tamper-evident signing reserved for regulated-vertical follow-up. |
| 10 | Termination defaults | `max_iter: 8`, `timeout: 60s`, configurable per-agent in `AgentLimits`. | Prevents runaway loops without surprising operator. |
| 11 | Observability | Existing `greentic-telemetry` OpenTelemetry integration. Spans: `aw.step` → `aw.llm_call`, `aw.tool_call`. Per-tenant attrs. | Reuse, no new infra. |

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
        │   ├── state.rs              ConversationState + AgentStateStore trait
        │   ├── state_redis.rs        RedisAgentStateStore (default impl)
        │   ├── tools.rs              Tool resolution + dispatch
        │   ├── llm.rs                LlmBackend trait + OpenAI default impl
        │   ├── tenant.rs             TenantContext
        │   ├── telemetry.rs          OTel span emission helpers
        │   └── error.rs              AgentError, TerminationReason
        └── tests/                    Unit (mocks) + integration (real Redis)
```

`greentic-aw-runtime` depends on:
- `greentic-ext-runtime` for `ExtensionRuntime::invoke_tool` (tool dispatch)
- `greentic-state` for Redis pool reuse
- `greentic-telemetry` for OTel
- Standard async libs (tokio, async-trait, serde, serde_json, anyhow, thiserror)

Does NOT depend on:
- `greentic-runner-host` (consumed by it, not consumer of it)
- `greentic-flow` (no flow model leakage)
- `greentic-designer` (designer pulls aw-runtime as workspace-external git dep)

### 5.2 Core API surface

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

#[async_trait]
pub trait ConfigProvider: Send + Sync {
    async fn agent_config(
        &self,
        tenant: &TenantContext,
        agent_id: &str,
    ) -> Result<AgentConfig, ConfigError>;
}

#[async_trait]
pub trait AgentStateStore: Send + Sync {
    async fn load(&self, tenant: &TenantContext, session_id: &str)
        -> Result<ConversationState, StateError>;
    async fn save(&self, tenant: &TenantContext, session_id: &str, state: &ConversationState)
        -> Result<(), StateError>;
    async fn acquire_lock(&self, tenant: &TenantContext, session_id: &str, wait: Duration)
        -> Result<SessionLock, StateError>;
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;
}

pub trait Telemetry: Send + Sync {
    fn record_step(&self, ctx: &StepTelemetryCtx);
    fn record_llm_call(&self, ctx: &LlmCallTelemetryCtx);
    fn record_tool_call(&self, ctx: &ToolCallTelemetryCtx);
}
```

### 5.3 Plan-Act-Observe loop

Pseudocode:

```
step(tenant, session_id, agent_id, message):
  _lock = state_store.acquire_lock(tenant, session_id, 5s)
  config = config_provider.agent_config(tenant, agent_id)         # cached 60s
  state  = state_store.load(tenant, session_id)                    # or init
  state.messages.push(User(message.text))

  for iter in 0..config.limits.max_iter:
    if iter elapsed > config.limits.timeout: break(Timeout)

    tools_schema = ext_runtime.list_tools(config.tools)
    request = LlmRequest { system_prompt, history: state.messages, tools: tools_schema }
    response = llm.complete(request) with retry(3, exp_backoff)

    match response:
      ToolCalls(calls):
        for call in calls:
          if not allowed(call.name, config.tools):
            state.messages.push(Tool(error="not allowed"))
            continue
          result = ext_runtime.invoke_tool(call.ext_id, call.name, call.args)
          state.messages.push(Tool(result))
          trail.push(ToolCall(...))
        continue loop
      FinalReply(text):
        state.messages.push(Assistant(text))
        trail.push(Reply(text))
        break(FinalReply)

  if iter >= max_iter: break(MaxIterations)

  truncate(state.messages, config.limits.max_history_turns)
  state_store.save(tenant, session_id, state)
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

### 5.5 Redis key schema

```
aw:{tenant}:{env}:{session}:state           ConversationState (JSON-encoded)
aw:{tenant}:{env}:{session}:lock            Mutex SET NX (TTL 30s)
aw:{tenant}:{env}:meta:{agent_id}:config    Cached AgentConfig (TTL 60s)
aw:{tenant}:{env}:audit:{session}:{step_id} Audit step (optional, feature flag)
```

Distinct from `greentic-state` flow-state namespace; no collision.

## 6. Termination & Error Semantics

| Termination | Cause | AgentOutput.terminated_by |
|---|---|---|
| `FinalReply` | LLM emits text without tool calls | OK path |
| `MaxIterations` | Loop count ≥ `config.limits.max_iter` (default 8) | Reply = last partial / "I need more time" stub |
| `Timeout` | Wall-clock ≥ `config.limits.timeout` (default 60s) | Reply = timeout-aware message |
| `Error(reason)` | LLM 5xx after retries, state load failure (recoverable), unhandled exception | Caller surfaces error message |
| `Cancelled` | (Post-MVP) external cancel | n/a MVP |

Tool dispatch errors are NOT termination — the error becomes an observation message and the agent retries.

State load failure → init empty state with warning log; user message preserved. State save failure → return reply but log error; next turn rebuilds from Redis snapshot or starts fresh (acceptable, rare).

## 7. Tenant Isolation & Security

- `TenantContext` mandatory on every public method. Compile-time enforced.
- Redis keys prefixed with `tenant_id` + `env`. No cross-tenant access path.
- Agent config resolved per-tenant via `ConfigProvider`. Config not shared across tenants.
- Tool dispatch via `ExtensionRuntime::invoke_tool` already tenant-scoped (existing greentic behaviour).
- LLM provider credentials never persisted in `ConversationState` or `AgentConfig` cache; resolved fresh per call via admin config.
- Future hooks (not enforced MVP):
  - Per-tenant LLM token budgets (telemetry records, admin enforces)
  - Per-tenant rate limits (record only)
  - Audit signing / hash chain (regulated-vertical post-MVP)

## 8. Test Plan

### 8.1 Unit tests (in-crate `tests` dir or `#[cfg(test)]`)

- `step` happy path: user → LLM emits text → reply
- `step` with one tool call: LLM emits tool call → tool returns → LLM emits final reply → reply
- `step` max_iter termination: LLM keeps emitting tool calls indefinitely, terminate at 8
- `step` timeout: mock LLM hangs, terminate at 60s
- `step` tool not allowed: LLM calls tool not in config, agent observes error, retries, eventually replies
- `step` LLM 5xx with retry: backend errors twice, succeeds third → completes
- `step` state load failure: store returns error, step proceeds with empty state, save still attempted
- `step` truncation: history > max_turns triggers oldest-pair drop, system prompt preserved
- `TenantContext` propagation: every internal call carries tenant; mocks assert

### 8.2 Integration tests (`tests/`)

- Real Redis: multi-turn conversation persists across `step()` calls
- Redis: concurrent `step()` on same session blocked by lock
- Redis: TTL refresh on save
- Multi-tenant: two tenants in same Redis, verify zero cross-talk
- Real `ExtensionRuntime` (test harness): tool call dispatched + observation returned

### 8.3 Designer playground end-to-end

- Test mode invokes `/api/chat` → real `AgentRuntime` → reply roundtrip
- `DwFormState` change → next-turn behaviour reflects change
- Trail surfaces in UI panel (assertion: panel populated)

### 8.4 Runner end-to-end

- Flow definition: `WebChat → DwAgent → reply`, mock LLM via env var
- Verify reply traverses graph, telemetry spans emitted, state persists

### 8.5 Acceptance criteria (MVP ship)

1. ✅ Flow with `WebChat → DwAgent → reply` deployable + functional
2. ✅ Designer test playground produces identical agent behaviour to deployed flow
3. ✅ Multi-tenant: 2 tenants in same Redis cluster, zero cross-talk verified
4. ✅ Plan-Act-Observe with 1+ tool call observed end-to-end
5. ✅ Max iter + timeout enforcement verified
6. ✅ State persists across runner restart
7. ✅ Telemetry traces visible per agent step
8. ✅ Banking-id composer's generated config → real reply (composer + AW pipeline integrated)

## 9. Architectural Seeds for Future Scale

Decisions designed so future work doesn't require rewrite:

| Future need | Seed in MVP |
|---|---|
| Extract AW runtime to standalone service | All async traits + `Send + Sync`; library wraps as RPC client/server later |
| DwBase atom backend | `AgentStateStore` trait swap, no library or caller change |
| Multi-provider LLM routing per tenant | `LlmBackend` trait; route inside backend impl |
| Per-tenant cost/budget enforcement | `Telemetry::record_llm_call` already emits per-tenant; enforcement plugs into telemetry sink |
| Per-region deployment / data residency | TenantContext.env + smarter ConfigProvider/StateStore impls |
| Streaming SSE responses | `step()` returns `AgentOutput`; add `step_stream()` parallel API later |
| Cooperative cancel mid-loop | Add `CancelToken` parameter, library checks at iteration boundaries |
| Agent reflection / multi-agent | Loop variant in `loop.rs`, new `step_reflect()` API |

## 10. Non-Goals

- **No** new ops infrastructure (uses existing Redis cluster, existing OTel collector)
- **No** changes to existing `FlowEngine` node types (only adds `DwAgent`)
- **No** changes to admin designer config schema beyond what already exists (`LlmProviderRef`)
- **No** breaking changes to composer extension interface (`greentic:dw-composer/composer@0.1.0`)
- **No** new auth/identity model — TenantContext is read from existing flow/designer auth

## 11. Open Questions

- Final `AgentConfig` schema details (e.g., should `tools` reference extension IDs by full path or short alias? — punt to plan phase)
- Cached config invalidation: simple TTL OR pub/sub invalidation? — TTL for MVP, pub/sub deferred
- Redis cluster sharing with `greentic-state`: same instance + key namespace separation, OR separate instance? — same instance with namespace OK for MVP
- Error message UX: surface raw `AgentError::Display` in chat reply, OR sanitised for end-user? — caller (runner/designer) decides per surface

## 12. References

- Mono-workspace CLAUDE.md: `/Users/bimapangestu/Desktop/Works/personal/greentic/CLAUDE.md`
- `greentic-dwbase` workspace: storage substrate (not executor) — future state backend
- `greentic-ext-runtime`: tool dispatch via `invoke_tool`
- `greentic-state` + `greentic-session`: Redis pattern reuse
- `greentic-telemetry`: OTel integration
- `greentic.dw.composer.banking-id-1.3.0-research`: prototype composer that produces `AnswerDocument` consumed by this runtime
- Greentic Labs landing: `https://greentic-labs.lovable.app/` (target customer positioning)
