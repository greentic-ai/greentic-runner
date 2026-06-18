# Agentic Worker — External Guardrails & Context-Window Management

- **Date:** 2026-06-18
- **Status:** Design / discussion (guardrail PoC scoped for implementation; context-window section is options-only)
- **Scope:** `greentic-runner` — `crates/greentic-aw-runtime/`, `crates/greentic-runner-host/`
- **Audience:** Andy (review), AW maintainers

## 1. Background & questions

Andy raised two questions about the agentic worker (AW):

1. **External guardrails** — can the AW integrate external guardrail services (AWS Bedrock Guardrails, Cisco AI Defense, Azure AI Content Safety, …)? We do not need to integrate all of them; we need to **prove a guardrail can be plugged in**, with one working demo (AWS Bedrock Guardrails).
2. **Context-window management** — what can be done? This is a discussion of options, not a committed implementation.

This document records the AW as it exists today, proposes a guardrail seam plus a Bedrock PoC, and lays out a phased plan for context-window management.

## 2. Current state (verified against code)

### 2.1 The Plan-Act-Observe loop and the LLM seam

- The agent loop lives in `crates/greentic-aw-runtime/src/loop.rs`. Each iteration builds an `LlmRequest { system_prompt, history, tools }` (`loop.rs:150-155`) and calls `LlmBackend::complete()` / `complete_streaming()`.
- `LlmBackend` is a trait (`crates/greentic-aw-runtime/src/llm.rs:50-75`) and is **already composed with a decorator** — `RetryingLlmBackend` (3 attempts, exponential backoff). The backend is selected in `crates/greentic-runner-host/src/runner/agent_node.rs::build_llm_backend` between a native `OpenAiLlmBackend` and an `ExtensionLlmBackend` (LLM-as-WASM-extension bridge).
- **There is no input or output filtering anywhere.** No validation before the LLM call, no filtering after, no per-step interceptor beyond the tool allow-list (`loop.rs:221`).

The decorator pattern at the `LlmBackend` seam is the established, low-blast-radius place to add guardrails: a wrapper composes cleanly with `RetryingLlmBackend`, and `loop.rs` needs essentially no change.

### 2.2 Extension mechanism

- Extensions are WASM components (`wasm32-wasip2`) exporting `greentic:extension-base` (`manifest` + `lifecycle`). The extension `kind` enum is **closed**: `design | bundle | deploy | provider` (`greentic-designer-extensions/wit/extension-base.wit:10-15`).
- Adding a brand-new `guardrail` kind means changing the WIT enum, defining a new WIT interface, adding runner dispatch, and standing up a new extension repo — heavyweight for a PoC. SigV4-signed egress to AWS from inside a WASM sandbox is also awkward.
- Important precedent: `ExtensionLlmBackend` shows the canonical "trait in core + bridge to WASM" shape. We mirror it for guardrails so the trait introduced now is forward-compatible with a future WASM guardrail kind.

### 2.3 Context / conversation state

- Conversation history is a `Vec<ChatMessage>` persisted to Redis and **cloned in full into every LLM turn** (`crates/greentic-aw-runtime/src/state.rs:32`, `loop.rs:152`).
- The **only** context management today is turn-count truncation: `AgentLimits.max_history_turns` (default 20, `config.rs:111-114`), applied by `state.truncate_history()` (`state.rs:52-77`) **after** a step completes (`loop.rs:323`) — so the full pre-truncation history is still what got sent this turn. System messages are preserved.
- **Not present:** token counting, per-context token budget, model→context-limit map, sliding-window-by-tokens, summarization/compaction, tool-result pruning. The model is a free-form string in `LlmProviderRef` (`config.rs:20-29`); context overflow surfaces as a raw provider error.
- Adjacent, already-wired context sources: long-term memory (`long_term.rs`, injected into the system prompt at `loop.rs:67-81`) and Knowledge/RAG (`knowledge.rs`, injected at `loop.rs:87-102`). A short-term `MemoryProvider` trait exists (`memory.rs:37-51`) but is **not** wired into the loop.
- Centralized insertion point for any context strategy: the `LlmRequest` assembly at `loop.rs:150-155`.

## 3. Decisions (locked with Andy)

| Decision | Choice |
| --- | --- |
| Architecture | Trait `Guardrail` in `aw-runtime` + decorator `GuardrailingLlmBackend`; native AWS Bedrock demo backend; WASM guardrail kind documented as the future production path. |
| Checkpoints in PoC | **Input** (user → LLM) and **Output** (LLM → user). |
| Action on violation | **Block + safe message.** |
| Context-window | Discussion only in this doc; not in the PoC. |

## 4. Guardrail design

### 4.1 Core trait (`crates/greentic-aw-runtime/src/guardrail.rs`, new)

Mirrors the async-trait-object style of `LlmBackend`.

```rust
pub enum GuardrailStage { Input, Output }

pub enum GuardrailAction {
    Allow,
    Block { message: String },   // used by the PoC
    Mask  { text: String },      // defined now, unused by the PoC
}

pub struct GuardrailVerdict {
    pub action: GuardrailAction,
    /// Raw vendor assessment detail, forwarded to telemetry/trace.
    pub assessments: serde_json::Value,
}

#[derive(thiserror::Error, Debug)]
pub enum GuardrailError { /* transport, auth, config, … */ }

pub trait Guardrail: Send + Sync {
    fn check<'a>(
        &'a self,
        stage: GuardrailStage,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<GuardrailVerdict, GuardrailError>> + Send + 'a>>;
}
```

A `NoopGuardrail` returning `Allow` is the global default, so the feature is zero-impact when disabled.

**Fail-open vs fail-closed:** on `GuardrailError` (network/auth failure), the PoC **fails open** (treat as `Allow`) and records the error to telemetry. Fail-closed is a one-line policy flip and is noted as a config option for production; defaulting open keeps a guardrail outage from taking the worker down during the demo.

### 4.2 Decorator `GuardrailingLlmBackend`

Wraps `Arc<dyn LlmBackend>` + `Arc<dyn Guardrail>`. All logic lives at the LLM seam; `loop.rs` is unchanged except wiring.

- **INPUT check:** before `inner.complete()`, only when the last history entry is a `User` message (i.e. the first LLM call of a turn — subsequent iterations end in a `Tool` result, so we do not re-scan the same user text). On `Block`, short-circuit and return a synthetic `LlmResponse` whose content is the safe message and whose `tool_calls` is empty, so the loop terminates cleanly.
- **OUTPUT check:** after `inner.complete()`, scan `response.content`. On `Block`, replace `content` with the safe message and drop any `tool_calls`.
- **Compose order:** `GuardrailingLlmBackend( RetryingLlmBackend( <OpenAi|Extension> ) )` — guardrail sits outside retry so it evaluates the final text after retries settle.
- **Streaming (`complete_streaming`):** input check is identical. Output is checked on the accumulated text once the stream finishes. **Caveat (documented):** with streaming, a blocked reply may already have partially streamed to the client before the verdict is known; mitigations (buffer-until-verdict, or post-hoc redaction event) are listed as follow-ups and out of PoC scope.

### 4.3 Demo backend `AwsBedrockGuardrail` (feature `guardrail-bedrock`)

- Calls **`bedrock-runtime:ApplyGuardrail`** — `POST /guardrail/{guardrailIdentifier}/version/{guardrailVersion}/apply`, body `{ "source": "INPUT"|"OUTPUT", "content": [ { "text": { "text": "<payload>" } } ] }`, SigV4-signed.
- Uses `aws-config` + `aws-sdk-bedrockruntime` for correct credential resolution and SigV4. **Feature-gated** (`guardrail-bedrock`) so default builds pull none of the AWS SDK weight.
- Response mapping:
  - `action == "GUARDRAIL_INTERVENED"` → `GuardrailAction::Block { message }` (message from the guardrail's configured `blockedMessaging`, or a static fallback).
  - `action == "NONE"` → `Allow`.
  - `assessments` array → forwarded verbatim into `GuardrailVerdict.assessments`.
- No `unwrap()` / `panic!()`; errors via `thiserror` → `GuardrailError`.

### 4.4 Wiring & configuration (`agent_node.rs::build_llm_backend`)

Opt-in, default off. New environment variables:

| Var | Meaning |
| --- | --- |
| `GREENTIC_AW_GUARDRAIL` | `bedrock` to enable the Bedrock backend; unset/`noop` → `NoopGuardrail`. |
| `GREENTIC_AW_GUARDRAIL_ID` | Bedrock guardrail identifier. |
| `GREENTIC_AW_GUARDRAIL_VERSION` | Guardrail version (`DRAFT` or a published number). |
| `GREENTIC_AW_GUARDRAIL_FAILMODE` | `open` (default) or `closed`. |
| `AWS_REGION`, `AWS_ACCESS_KEY_ID`, … | Standard AWS credential chain. |

When `GREENTIC_AW_GUARDRAIL` is set, `build_llm_backend` constructs the guardrail, then wraps the (already-retrying) backend in `GuardrailingLlmBackend`.

### 4.5 Telemetry

Guardrail verdicts (stage, action, assessment summary) are emitted through the existing `Telemetry` / `StepObserver` path (`lib.rs:97-114`) as a trace event, so a blocked turn is observable end-to-end.

### 4.6 Tests

- `NoopGuardrail` passes input/output through unchanged.
- `GuardrailingLlmBackend` with a mock guardrail that blocks on a keyword: asserts (a) input block short-circuits without calling the inner backend, and (b) output block replaces content and drops tool_calls.
- `AwsBedrockGuardrail`: unit test for response parsing (intervened / none / masked); an integration test gated `#[ignore]` requiring real AWS credentials and a provisioned guardrail.

### 4.7 Demo script for Andy

```bash
export GREENTIC_AW_GUARDRAIL=bedrock
export GREENTIC_AW_GUARDRAIL_ID=<id>
export GREENTIC_AW_GUARDRAIL_VERSION=DRAFT
export AWS_REGION=us-east-1   # + credentials
# build with: cargo build -p greentic-runner-host --features guardrail-bedrock
```

Send a prompt that trips the configured policy (e.g. a denied topic or PII). The worker returns the guardrail's safe message instead of the model's answer; the trace shows the Bedrock `GUARDRAIL_INTERVENED` assessment.

### 4.8 Future production-native path (documented, NOT implemented)

A WASM `guardrail` extension kind is the long-term Greentic-native form:

1. Add `guardrail` to the `kind` enum (`extension-base.wit:10-15`).
2. Define `greentic:extension-guardrail` WIT (`check-input` / `check-output`).
3. Add runner dispatch (mirroring `invoke_tool`).
4. Vendors (Cisco AI Defense, Bedrock, Azure Content Safety) ship signed WASM components like every other extension.

The `Guardrail` trait introduced here then gains a second implementation — `ExtensionGuardrail`, bridging to the WASM component — **exactly mirroring `ExtensionLlmBackend`**. So today's trait is the stable seam; any vendor is "just another `Guardrail` impl," native or WASM.

## 5. Context-window management (options)

Ranked by ROI / risk, framed as phases. None is in the guardrail PoC.

### Phase 1 — Token-aware sliding window (recommended first)
- Add token counting (`tiktoken-rs` for OpenAI-family; an approximate counter for others) and a `model → context_limit` lookup.
- Compute a budget: `context_limit − reserved_output − tokens(system_prompt) − tokens(tools_schema)`. Trim oldest non-system messages until the history fits, preserving recent turns.
- Apply at the centralized seam `loop.rs:150` (before the request is built), as a `ContextStrategy` field on `AgentLimits` (`config.rs`). This augments/replaces today's after-the-fact turn-count truncation.
- Low risk, single insertion point, biggest immediate payoff (prevents hard context-overflow errors).

### Phase 2 — Rolling summarization / compaction
- When the budget is exceeded, summarize the oldest N turns into one synthetic "conversation summary" message via a cheap model call, persist it in state, and replace those turns. Recursive as the conversation grows.
- Costs one extra LLM call when triggered; cache the summary so it is computed once.

### Phase 3 — Offload to existing memory/RAG
- Greentic already injects long-term memory (`long_term.rs`) and Knowledge/RAG (`knowledge.rs`) into the system prompt. Strategy: keep the working window small and push older context to long-term memory, recalling on demand. The seams already exist; this is mostly a policy/config change.

### Supporting items
- **Tool-result pruning:** large tool outputs are dropped or summarized after they have been consumed, keeping only a compact reference.
- **Per-turn token budget + telemetry:** count tokens before sending, emit near-limit metrics, and surface warnings. Ties into `TokenMeter` (`cost.rs`), which today only tracks daily per-tenant billing.

**Recommendation:** ship Phase 1, make Phase 2 opt-in, and lean on existing infrastructure for Phase 3.

## 6. Out of scope

- Implementing Cisco AI Defense / Azure Content Safety backends (the trait makes them straightforward follow-ups).
- The WASM `guardrail` extension kind (documented as the future path only).
- Any context-window implementation (this doc only discusses options).

## 7. Open questions

- Which AWS account / region and guardrail policy will back the live demo? (Needs a provisioned Bedrock guardrail ID + credentials — devops.)
- Production default for guardrail fail mode: open or closed?
- Should the streaming output path buffer-until-verdict in production, accepting added latency, or stream-then-redact?
