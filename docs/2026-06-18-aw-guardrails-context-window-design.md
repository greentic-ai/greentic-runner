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
| Checkpoints in PoC | **Input** (user → LLM), **Output** (LLM → user), **Tool-result** (tool output before it re-enters history), and **Tool-call args** (folded into the Output check). See §4.2 for why tool-result is in scope despite the original "Input + Output" framing. |
| Action on violation | **Block + safe message** for denied-topic / content-filter / word policies; **Mask-and-continue** for sensitive-information (PII) policies, reusing Bedrock's redacted `outputs[].text`. The action is derived from the Bedrock assessment, not hardcoded (§4.3). |
| Fail mode | Default **fail-open** for the demo; **fail-closed recommended for the INPUT/tool-result stages in production** (see §4.1). |
| Mask persistence | When masking, the **masked form is persisted** to conversation state (not the original), so PII does not re-enter context on later turns (§4.2). |
| Context-window | Discussion only in this doc; not in the PoC. |

> **Decision-change note:** the original lock with Andy was "Input + Output, Block + safe message." Code review surfaced that (a) tool results bypass both checks and (b) Bedrock returns masked text for free. The scope above is the revised decision: tool-result + tool-call coverage added, and Mask wired alongside Block. Block remains the default for non-PII policies, so this is an extension of the original decision, not a reversal.

## 4. Guardrail design

### 4.1 Core trait (`crates/greentic-aw-runtime/src/guardrail.rs`, new)

Mirrors the async-trait-object style of `LlmBackend`.

```rust
pub enum GuardrailStage { Input, Output }

pub enum GuardrailAction {
    Allow,
    Block { message: String },   // denied-topic / content-filter / word policies
    Mask  { text: String },      // sensitive-info (PII) redaction — Bedrock outputs[].text
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

**Fail-open vs fail-closed:** on `GuardrailError` (network/auth failure), the PoC **fails open** (treat as `Allow`) and records the error to telemetry — a guardrail outage should not take the worker down during the demo. The fail mode is configurable per the env var in §4.4.

**Production recommendation:** prefer **fail-closed for the INPUT and tool-result stages**. A guardrail outage frequently coincides with an attack window (an attacker may even be the cause), so failing open on the *ingress* side is exactly when it hurts. Failing open on the OUTPUT stage is the milder choice (worst case: an unfiltered model reply, no externally-injected payload). The PoC keeps the global default open for demo smoothness but documents the split so production can flip INPUT/tool-result to closed.

### 4.2 Decorator `GuardrailingLlmBackend`

Wraps `Arc<dyn LlmBackend>` + `Arc<dyn Guardrail>`. The decorator covers three of the four checkpoints purely at the LLM seam (`loop.rs` unchanged except wiring); the **tool-result** checkpoint cannot be reached from the `LlmBackend` seam and needs one small hook in `loop.rs` (see below).

- **INPUT check:** before `inner.complete()`, scan the last `User` message in `request.history`. On `Block`, short-circuit and return a synthetic `LlmResponse` whose content is the safe message and whose `tool_calls` is empty, so the loop terminates cleanly. On `Mask`, replace the user text passed downstream with the masked text.
- **OUTPUT check:** after `inner.complete()`, scan a serialization of **both** `response.content` **and** the `tool_calls` (call name + JSON args). Scanning the args closes the exfil-via-tool-argument vector: a model can place sensitive payload in a tool argument, and content-only scanning would miss it. On `Block`, replace `content` with the safe message and drop `tool_calls`. On `Mask`, apply Bedrock's redacted text to `content`.
- **Compose order:** `GuardrailingLlmBackend( RetryingLlmBackend( <OpenAi|Extension> ) )` — guardrail sits outside retry so it evaluates the final text after retries settle.
- **Streaming (`complete_streaming`):** input check is identical. Output is checked on the accumulated text once the stream finishes. **PoC default = stream-then-redact:** deltas stream to the client as they arrive and the OUTPUT verdict applies to the accumulated text at the end, so a blocked reply may already have partially streamed before the verdict is known. The stricter alternative (buffer-until-verdict, full enforcement at the cost of latency) is an open question (§8), not the demo default.

#### Persisting masked content (Mask + multi-turn correctness)

`Mask` has a persistence consequence that `Block` does not. The decorator only transforms the *outbound request*; meanwhile the original user message is appended to `state.messages` at `loop.rs:57-59` **before** the LLM call, and `loop.rs:324` persists `state.messages` to Redis. So a decorator-level input mask would hide PII from the model *this turn* but leave the original in persisted history — the PII re-enters context next turn and the mask is defeated.

**Decision: persist the masked form** for both tool-result and input, so PII does not re-enter context on later turns. Implications:
- **Tool-result:** the `loop.rs` hook runs before the append, so it naturally persists the masked/placeholder value — no extra work.
- **Input:** to persist masked, the input-stage mask must write the masked text back into `state.messages` (the last `User` entry) in `loop.rs`, not only into the outbound request. This is a second small `loop.rs` touch, parallel to the tool-result hook.
- **Trade-off accepted:** the original text is not retained in conversation state (it may still exist in upstream provider logs). This can cause mild multi-turn awkwardness (the model sees `[REDACTED]` where a value once was), which is preferable to silently re-leaking PII. Strict tenants can set `GREENTIC_AW_GUARDRAIL_PII=block` to avoid masked history entirely.

#### Latency cost (acknowledged, not optimized in PoC)

Each checkpoint is a serial SigV4 round-trip to Bedrock **on the critical path** (unlike telemetry, which is fire-and-forget). A single tool-using turn can add 3–4 serial `ApplyGuardrail` calls (input + one per tool-result + output). For the PoC this is acceptable and the demo will report measured latency, but production will likely want batching, parallelizing independent checks, and the trusted-tool exemption (§5) to keep per-turn overhead bounded. **Latency is not yet measured;** that is an open question (§8).

#### Tool-result checkpoint (the fourth checkpoint — needs a `loop.rs` hook)

In the agentic loop, the highest-risk untrusted content is **tool output** (web fetch, MCP call, component result): it is both the primary prompt-injection vector and a PII-exfiltration vector. This content is invisible to the `LlmBackend` decorator because:

- it enters as a `ChatMessage::Tool` appended to history after dispatch (`loop.rs:210-213, 297-300`), so on the next iteration `history.last()` is `Tool`, not `User`, and the INPUT gate skips it; and
- the OUTPUT check only sees the *model's* reply, not the raw tool result that shaped it.

So the PoC adds a minimal INPUT-stage guardrail call in `loop.rs` immediately after a tool result is produced and **before** it is appended to history. On `Block`, the tool result is replaced with a safe placeholder value (e.g. `{"error":"blocked by guardrail policy"}`) so the loop continues without the offending content. On `Mask`, the redacted text replaces the tool content. This is the one place the guardrail reaches into `loop.rs` rather than living purely at the LLM seam; it is intentional and small (a single call guarded by an `Option<Arc<dyn Guardrail>>` on the runtime, defaulting to `None`).

### 4.3 Demo backend `AwsBedrockGuardrail` (feature `guardrail-bedrock`)

- Calls **`bedrock-runtime:ApplyGuardrail`** — `POST /guardrail/{guardrailIdentifier}/version/{guardrailVersion}/apply`, body `{ "source": "INPUT"|"OUTPUT", "content": [ { "text": { "text": "<payload>" } } ] }`, SigV4-signed.
- Uses `aws-config` + `aws-sdk-bedrockruntime` for correct credential resolution and SigV4. **Feature-gated** (`guardrail-bedrock`) so default builds pull none of the AWS SDK weight.
- Response mapping (a pure function `map_apply_guardrail` so it is unit-testable without AWS — the SDK call only produces its inputs):
  - `action == "NONE"` → `Allow`.
  - `action == "GUARDRAIL_INTERVENED"`:
    - If the **only** intervening assessment is a sensitive-information policy that *anonymized* (masked) content — i.e. Bedrock returned redacted text in `outputs[].text` rather than a hard block — → `Mask { text: outputs[0].text }`.
    - Otherwise (denied topic, content filter, word policy, or a blocking sensitive-info action) → `Block { message }`, where `message` is `outputs[0].text` if present (Bedrock substitutes the configured blocked message there) else a static fallback.
  - `assessments` → summarized into `GuardrailVerdict.assessments` (the SDK assessment types are not `Serialize`; the PoC records a compact JSON summary — intervening policy kinds + counts — sufficient for telemetry). **Compliance note:** this summary is enough for operational visibility but not for a full audit trail. If a tenant later needs forensic detail, the raw Bedrock response should be persisted out-of-band (e.g. an audit sink) rather than expanding the in-line verdict; called out as a follow-up, not PoC scope.
- A config toggle `GREENTIC_AW_GUARDRAIL_PII=mask|block` (default `mask`) lets operators force PII findings to hard-block instead of redacting, for stricter environments.
- No `unwrap()` / `panic!()`; errors via `thiserror` → `GuardrailError`.

### 4.4 Wiring & configuration (`agent_node.rs::build_llm_backend`)

Opt-in, default off. New environment variables:

| Var | Meaning |
| --- | --- |
| `GREENTIC_AW_GUARDRAIL` | `bedrock` to enable the Bedrock backend; unset/`noop` → `NoopGuardrail`. |
| `GREENTIC_AW_GUARDRAIL_ID` | Bedrock guardrail identifier. |
| `GREENTIC_AW_GUARDRAIL_VERSION` | Guardrail version (`DRAFT` or a published number). |
| `GREENTIC_AW_GUARDRAIL_FAILMODE` | `open` (default) or `closed`. Production should set `closed` (applies to INPUT + tool-result stages; OUTPUT stays open). |
| `GREENTIC_AW_GUARDRAIL_PII` | `mask` (default) or `block` — whether PII findings redact-and-continue or hard-block. |
| `AWS_REGION`, `AWS_ACCESS_KEY_ID`, … | Standard AWS credential chain. |

When `GREENTIC_AW_GUARDRAIL` is set, `build_llm_backend` constructs the guardrail, then wraps the (already-retrying) backend in `GuardrailingLlmBackend`.

### 4.5 Telemetry

Guardrail verdicts (stage, action, assessment summary) are emitted through the existing `Telemetry` / `StepObserver` path (`lib.rs:97-114`) as a trace event, so a blocked turn is observable end-to-end.

### 4.6 Tests

- `NoopGuardrail` passes input/output through unchanged.
- `GuardrailingLlmBackend` with a mock guardrail that blocks on a keyword: asserts (a) input block short-circuits without calling the inner backend, (b) output block replaces content and drops tool_calls, and (c) a keyword placed only in a **tool-call argument** (not in `content`) is still detected and blocks — proving args are scanned.
- `GuardrailingLlmBackend` with a mock guardrail that masks: asserts the masked text replaces the user/assistant text and the turn continues (no short-circuit).
- Tool-result hook (`loop.rs`): a mock guardrail that blocks on a keyword in a tool result asserts the offending tool content is replaced with the safe placeholder before it re-enters history.
- Mask persistence (`loop.rs`): a mock guardrail that masks asserts the **persisted** `state.messages` entry holds the masked text (not the original), for both an input user message and a tool result — proving PII does not re-enter context next turn.
- `map_apply_guardrail` (pure): unit tests for `NONE → Allow`, denied-topic `INTERVENED → Block`, and PII-anonymized `INTERVENED → Mask` with the `GREENTIC_AW_GUARDRAIL_PII=block` override forcing `Block`.
- `AwsBedrockGuardrail`: an integration test gated `#[ignore]` requiring real AWS credentials and a provisioned guardrail.

### 4.7 Demo script for Andy

```bash
export GREENTIC_AW_GUARDRAIL=bedrock
export GREENTIC_AW_GUARDRAIL_ID=<id>
export GREENTIC_AW_GUARDRAIL_VERSION=DRAFT
export AWS_REGION=us-east-1   # + credentials
# build with: cargo build -p greentic-runner-host --features guardrail-bedrock
```

Two demo paths against the configured policy:
- **Denied topic / content filter** → the worker returns the guardrail's safe message instead of the model's answer (Block).
- **PII in the reply** (with default `GREENTIC_AW_GUARDRAIL_PII=mask`) → the worker returns the answer with PII redacted (Mask-and-continue); set `=block` to demo a hard block instead.

In both cases the trace shows the Bedrock `GUARDRAIL_INTERVENED` assessment.

### 4.8 Future production-native path (documented, NOT implemented)

A WASM `guardrail` extension kind is the long-term Greentic-native form:

1. Add `guardrail` to the `kind` enum (`extension-base.wit:10-15`).
2. Define `greentic:extension-guardrail` WIT (`check-input` / `check-output`).
3. Add runner dispatch (mirroring `invoke_tool`).
4. Vendors (Cisco AI Defense, Bedrock, Azure Content Safety) ship signed WASM components like every other extension.

The `Guardrail` trait introduced here then gains a second implementation — `ExtensionGuardrail`, bridging to the WASM component — **exactly mirroring `ExtensionLlmBackend`**. So today's trait is the stable seam; any vendor is "just another `Guardrail` impl," native or WASM.

## 5. Context-window management (options)

Ranked by ROI / risk, framed as phases. None is in the guardrail PoC.

### Phase 1 — Token-aware budgeting: post-injection sizing + tool-result pruning + sliding window
This phase has two co-equal levers, because in practice context overflow is driven *more* by a single fat tool output (a 40k-token web fetch) than by a long chat history. Turn-based windowing does nothing for the former, so tool-result pruning is in Phase 1, not "supporting."

- **Token counting** with `tiktoken-rs` for OpenAI-family models; an approximate (chars/4) counter for others. A `model → context_limit` lookup is needed but is a maintenance/staleness hazard because the model is a free-form string (`config.rs:20-29`); **prefer reading the limit from provider metadata where the provider exposes it**, and treat the static table as a fallback with a conservative default for unknown models.
- **Counter-accuracy margin.** The approximate counter is coarse and per-model tokenizers differ, so an estimate can be wrong in either direction — under-estimating still overflows; over-estimating discards history that would have fit. Mitigation: apply a **larger safety headroom for models on the approximate counter** (e.g. reserve a bigger fraction of the limit) than for models with an exact tokenizer. The conservative default applies to *both* the context limit and the counting accuracy, not just the limit.
- **Budget computed AFTER injection.** The system prompt is augmented with long-term memory and Knowledge/RAG at `loop.rs:67-102`, and that injection is variable-size — it is frequently the bloat source, not the history. So token counting must happen on the *assembled* request, and the budget is:
  `budget_for_history = context_limit − reserved_output − tokens(augmented_system_prompt) − tokens(tools_schema)`.
  - Trim oldest non-system history until it fits the remaining budget, preserving recent turns.
  - **Fallback when the augmented system prompt alone blows the budget:** trimming history cannot help. In that case shrink the *injection* first — drop the lowest-scoring RAG chunks / oldest recalled memory facts — before trimming conversation history. This requires the strategy to run where the injected content is still separable from the base prompt (i.e. integrate with the augmentation step at `loop.rs:67-102`, not only at the request-assembly point `loop.rs:150`).
- **Tool-result pruning (Phase 1, not supporting):** when a tool returns a large payload, store the full result out-of-band (or in long-term memory) and inject only a compact summary + a handle into history once it has been consumed. This is the highest-ROI single lever against real-world overflow.
- Expose as a `ContextStrategy` field on `AgentLimits` (`config.rs`); augments/replaces today's after-the-fact turn-count truncation.

### Phase 2 — Rolling summarization / compaction
- When the budget is exceeded, summarize the oldest N turns into one synthetic "conversation summary" message via a cheap model call, persist it in state, and replace those turns. Recursive as the conversation grows.
- Costs one extra LLM call when triggered; cache the summary so it is computed once.
- **Composes with guardrails:** the summarization call is itself an LLM call and should run through the same `GuardrailingLlmBackend`, so summarized history cannot launder content past the guardrail. Order: context strategy (incl. summarization) decides the history, then the guardrail decorator checks what is sent — these do not conflict (see §6 note).

### Phase 3 — Offload to existing memory/RAG
- Greentic already injects long-term memory (`long_term.rs`) and Knowledge/RAG (`knowledge.rs`) into the system prompt. Strategy: keep the working window small and push older context to long-term memory, recalling on demand. The seams already exist; this is mostly a policy/config change.

### Supporting items
- **Per-turn token budget + telemetry:** count tokens before sending, emit near-limit metrics, and surface warnings. Ties into `TokenMeter` (`cost.rs`), which today only tracks daily per-tenant billing.

### Guardrail follow-up — per-tool guardrail policy (trusted-tool exemption)
Distinct from context-window work but recorded here because it shares the tool-result seam: the PoC scans **every** tool result, but most tools are internal/trusted (reading the operator's own DB, arithmetic). Scanning their output costs a Bedrock round-trip with no benefit — the real risk is tools that pull *untrusted external* content (web fetch, third-party MCP). A follow-up should add a per-tool allow/deny policy controlling which tool results are guardrailed, which also directly bounds the per-turn latency from §4.2. Out of PoC scope.

**Recommendation:** ship Phase 1 (post-injection budgeting **and** tool-result pruning together), make Phase 2 opt-in, and lean on existing infrastructure for Phase 3.

## 6. Composition & ordering

When both features ship, the per-turn order is coherent and non-conflicting:

1. **Context strategy** runs first (at `loop.rs:67-102` for injection sizing and `loop.rs:150` for history assembly) and decides *what* goes into the `LlmRequest`.
2. **Guardrail decorator** runs second, at the `LlmBackend` seam, and checks *what was actually assembled* — so `GuardrailingLlmBackend( RetryingLlmBackend( <backend> ) )` evaluates the post-trim, post-summarization request. The guardrail sits outside retry so it judges the final text.
3. **Tool-result guardrail** (the `loop.rs` hook) runs before tool output re-enters history, so context trimming never operates on un-checked tool content.

A Phase-2 summarization call is itself routed through the guardrail decorator, so summarized history cannot launder content past the guardrail.

## 7. Out of scope

- Implementing Cisco AI Defense / Azure Content Safety backends (the trait makes them straightforward follow-ups).
- The WASM `guardrail` extension kind (documented as the future path only).
- Any context-window implementation (this doc only discusses options).

## 8. Open questions

- Which AWS account / region and guardrail policy will back the live demo? (Needs a provisioned Bedrock guardrail ID + credentials — devops.)
- **Resolved by this revision:** tool-result and tool-call-arg scanning are in PoC scope (§4.2); production fail mode = closed on INPUT/tool-result, open on OUTPUT (§4.1); masked content is persisted (not the original) so PII does not re-enter context (§4.2).
- **Latency is not yet measured.** Each checkpoint is a serial Bedrock round-trip on the critical path; a tool-using turn adds 3–4 (§4.2). The demo will report measured per-turn overhead; production mitigation (batching / parallel checks / trusted-tool exemption, §5) is a follow-up. How much added latency per turn is acceptable?
- Should the streaming OUTPUT path buffer-until-verdict in production (added latency, full enforcement) or keep the PoC default stream-then-redact (lower latency, partial leakage)?
- For PII findings, is `mask` (default) the right organization-wide default, or should regulated tenants force `block`?
- Does the chosen LLM provider expose context-limit metadata we can read, or do we accept a static `model → context_limit` table with a conservative fallback?
