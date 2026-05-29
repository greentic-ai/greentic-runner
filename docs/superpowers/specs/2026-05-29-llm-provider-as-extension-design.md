# LLM Provider as an Extension (for the AW Runtime) — Design (Umbrella)

**Date:** 2026-05-29
**Status:** Brainstorm — for review
**Repos:** greentic-runner (`greentic-aw-runtime`), greentic-interfaces (WIT), greentic-designer-extensions (`greentic-ext-runtime`), component-llm-openai, greentic-designer (wiring)

## Problem

`greentic-aw-runtime` calls a **hardcoded** `OpenAiLlmBackend` (a Rust HTTP client) from its agent loop. Adding Anthropic means a second hardcoded `AnthropicLlmBackend`. This contradicts the platform's core principle — *"core platform must NEVER contain provider-specific code; everything non-core is a `.gtpack` extension"* — and creates redundancy: there are already up to **four** LLM code paths (the designer's Rig multi-provider, the designer's `LlmBackend` enum + the Slice-3 `AwBackendAdapter`, aw-runtime's hardcoded `OpenAiLlmBackend`, and the `greentic.llm-openai` flow-node extension), none of which is the agentic provider the runtime actually needs.

## Investigation findings (2026-05-29)

- **No tool-calling LLM extension interface exists today.**
  - `greentic:extension-design/tools@0.2.0` (`greentic-designer-extensions/.../wit/deps/extension-design/extension-design.wit`) is for regular tools: `invoke-tool(name, args-json) -> result<string>`. No system prompt / history / tool schemas / tool_calls.
  - `wasix:mcp@25.06.18` (`greentic-interfaces/wit/wasix-mcp@25.06.18.wit`) has `complete(completion-request) -> completion-response`, but it is **text-only** — no tool schemas in, no `tool_calls` out.
  - `greentic.llm-openai` is a **flow node** (single-shot chat), not an agentic provider.
  - `component-llm-openai/` is an **uninitialised submodule** (empty) — the intended-but-unbuilt OpenAI bridge.
- **The aw-runtime trait is already provider-agnostic & tool-calling-capable.** `greentic-aw-runtime/src/llm.rs`:
  ```rust
  pub struct LlmRequest { system_prompt, history: Vec<ChatMessage>, tools: Vec<LlmToolSchema>, provider: LlmProviderRef }
  pub struct LlmResponse { content: Option<String>, tool_calls: Vec<ToolCallRecord>, tokens_in, tokens_out }
  pub trait LlmBackend { fn complete(&self, req: LlmRequest) -> ...Result<LlmResponse, LlmError>; }
  ```
  The loop (`loop.rs`) only calls `llm.complete(req)`. The runtime **already holds the `ExtensionRuntime`** (used to dispatch tool calls) — the exact handle needed to also call an LLM extension. This matches the aw-runtime spec's own **Decision 5** ("provider-agnostic `LlmBackend`").

## Goal

Make the worker LLM an **installable extension**, so `greentic-aw-runtime` carries no provider-specific code. Adding/swapping providers = installing extensions. The existing `LlmBackend` trait stays the seam; a new `ExtensionLlmBackend` impl dispatches to the extension.

## Decisions

1. **Credentials: host resolves and passes them in the request** (chosen over secret-ref). The caller that builds `LlmRequest` (designer = the encrypted vault; production runner = its config/secrets) supplies `{provider, model, api_key, base_url}` to the extension. The extension is a **stateless bridge**. Rationale: simplest; uniform for playground + production; mirrors how the hardcoded backend already receives the key; the sandboxed WASM needs the key regardless to call the API. **Forward-compatible:** the credential record is shaped so an optional `secret_ref` (extension resolves via a host secrets capability) can be added later without breaking the contract.
2. **Interface shape — TWO candidate approaches; pick in review:**
   - **(A) Dedicated WIT `greentic:extension-llm@0.1.0`** — a first-class `llm-provider` interface (`complete(llm-request) -> result<llm-response>`), typed records for messages/tool-schemas/tool-calls/credential. Correct & idiomatic, but requires: new WIT package in greentic-interfaces (+ host & guest bindings) AND `greentic-ext-runtime` support to instantiate + call that export.
   - **(B) LLM-as-a-tool convention (Recommended first cut)** — the bridge extension exposes a normal tool named `complete` (existing `tools` interface) whose `args-json` is the `LlmRequest` JSON and whose result is the `LlmResponse` JSON. `ExtensionLlmBackend` calls the **already-generic** `invoke_tool(ext_id, "complete", request_json)` — **no new WIT, no ext-runtime change**. Untyped (JSON-by-convention) but ships the principle (LLM = extension) with minimal cross-repo surface, and can be promoted to (A) later behind the same `LlmBackend` seam.
   - **Recommendation:** ship **(B)** first to prove the architecture end-to-end cheaply and delete the hardcoded backend, then formalise as **(A)** if/when a typed contract earns its keep. The `LlmBackend`/`ExtensionLlmBackend` seam means (A) is a drop-in upgrade later.

## Architecture (assuming (B) first; (A) noted where it differs)

### Wire shape (shared by both)
`LlmRequest` JSON: `{ system_prompt, history:[ChatMessage], tools:[{extension_id, tool_name, description, parameters}], credential:{ provider, model, api_key, base_url? } }`.
`LlmResponse` JSON: `{ content?, tool_calls:[{call_id, extension_id, tool_name, args}], tokens_in, tokens_out }`.
(Tool function naming `"{extension_id}.{tool_name}"` already standard in `llm_openai.rs`.)

### Components
1. **`ExtensionLlmBackend`** (`greentic-aw-runtime/src/llm_extension.rs`, new) — impl `LlmBackend`:
   ```rust
   pub struct ExtensionLlmBackend { ext_runtime: Arc<ExtensionRuntime>, extension_id: String }
   // complete(): serialize LlmRequest -> invoke_tool(extension_id, "complete", json)
   //            via spawn_blocking (same pattern as tools.rs) -> deserialize LlmResponse.
   //            errors -> LlmError (BadRequest vs ServiceUnavailable where distinguishable).
   ```
   For (A) this calls a typed `ext_runtime.complete_llm(...)` instead of `invoke_tool`.
2. **Backend factory** in `AgentRuntime::new` (or `from_packs`): pick the backend by config. `LlmProviderRef { provider, model }` gains the notion of an extension provider — e.g. `provider = "extension:<ext-id>"` (or a dedicated config field `llm_extension: Option<String>`). When set → `ExtensionLlmBackend`; else → legacy hardcoded path (kept during migration). Loop code unchanged.
3. **Bridge component** — fill `component-llm-openai` (then a sibling `component-llm-anthropic`, or one multi-provider component switching on `credential.provider`). For (B): export a `complete` tool whose JSON in/out is the wire shape above; call the provider's chat-completions API with function-calling; map tool_calls back. Built for `wasm32-wasip2`.
4. **Callers supply the credential:**
   - **Designer playground:** the test-chat dispatcher resolves the selected vault credential (Slice 1-3, already built) → puts `{provider, model, api_key, base_url}` into the request. The Slice-3 `AwBackendAdapter` + the designer's own provider enum become **legacy** once the runtime routes through the extension (can be retired in a later cleanup; keep until the extension path is proven).
   - **Production runner-host:** resolves the agent's credential from its config/secrets and supplies it the same way.

### Data flow (designer playground, approach B)
```
form.llm.credentialRef -> fetch_decrypted (vault) -> {provider, model, api_key, base_url}
  -> AgentRuntime configured with ExtensionLlmBackend{ ext_id = "<llm bridge>" }
  -> loop builds LlmRequest (history+tools+credential) -> ExtensionLlmBackend.complete
  -> ExtensionRuntime.invoke_tool("<llm bridge>", "complete", request_json)  [WASM]
  -> bridge calls OpenAI/Anthropic API -> LlmResponse JSON -> loop continues
```

## Decomposition (slices, build order)

Each slice is independently shippable with its own spec + plan + PR (to each repo's `research`).

- **Slice 1 — `ExtensionLlmBackend` + factory (greentic-runner), approach B.** New `LlmBackend` impl dispatching via `ExtensionRuntime::invoke_tool(ext_id, "complete", json)`; factory in `AgentRuntime::new`. Tested against a **mock extension** (a test `ExtensionRuntime`/tool returning a scripted `LlmResponse` JSON) — proves dispatch + mapping with no real component. Legacy hardcoded path retained.
- **Slice 2 — OpenAI bridge component (`component-llm-openai`).** Implement the `complete` tool (wire shape above) calling OpenAI chat-completions with function-calling; `wasm32-wasip2`; conformance test against the JSON contract. End-to-end: a real worker runs on the extension.
- **Slice 3 — Wiring + credential plumbing.** Runner config + designer test-chat select the extension provider and pass the resolved credential; retire/deprecate the hardcoded `OpenAiLlmBackend` + the designer `AwBackendAdapter` once the extension path is the default.
- **Slice 4 — Anthropic (+ multi-provider) bridge.** Either a second component or one component switching on `credential.provider`. This is the original "Anthropic in production" goal, now done the right way.
- **(Optional later) Slice 5 — Formalise as dedicated WIT `greentic:extension-llm@0.1.0` (approach A):** typed interface in greentic-interfaces + `greentic-ext-runtime` host support + guest bindings; swap `ExtensionLlmBackend` to the typed call behind the unchanged trait seam.

## Error handling

Bridge/runtime errors map to `LlmError`: provider 4xx (auth/bad request) → `BadRequest` (no retry); transport/5xx/timeout → `ServiceUnavailable` (retried by `RetryingLlmBackend`); malformed extension output → `Decode`. The agent loop already turns tool/LLM failures into a sanitised user reply + (designer) a `[test-chat]` console log.

## Testing

- Slice 1: `ExtensionLlmBackend` unit/integration tests against a mock extension returning scripted `LlmResponse` JSON (tool_calls round-trip, error mapping). No network.
- Slice 2: component conformance test (JSON contract) + a wiremock-style HTTP test of the OpenAI call.
- Slice 3: designer test-chat route test selecting an extension provider (mock bridge) end-to-end.
- Slice 4: as Slice 2 for Anthropic.

## Security

Credentials are resolved host-side (designer vault decrypt / runner secrets) and passed into the sandboxed bridge per call — the key never lives in the component, the worker definition, or the pack (only a vault reference does). Forward-compatible with a secret-ref + host-secrets-capability model if per-call key passing is later deemed too broad.

## Risks & Mitigations

- **(B) is untyped (JSON convention).** Mitigation: a shared serde contract (reuse aw-runtime's `LlmRequest`/`LlmResponse` types serialised to JSON) + conformance tests; the `LlmBackend` seam lets us upgrade to typed WIT (A) without touching the loop.
- **`greentic-ext-runtime` may need a small addition** even for (B) if `invoke_tool` requires an "active design extension" context or capability gating for a non-design extension — verify during Slice 1; the LLM bridge may need to be registered as a callable extension kind.
- **Latency:** WASM dispatch adds <50ms vs a multi-second LLM call — negligible.
- **Streaming:** out of scope (the loop is synchronous per turn), same as the aw-runtime spec.
- **Migration:** keep the hardcoded backend + designer adapter until the extension path is proven, then remove in Slice 3 to avoid a flag-day.

## Relationship to existing work

- The **credentials vault (designer Slices 1–3, merged)** stays — it's the credential *source*; the extension consumes the resolved credential.
- The **designer `AwBackendAdapter` (Slice 3)** + aw-runtime **`OpenAiLlmBackend`** become **legacy** once the runtime routes LLM through the extension; retired in this project's Slice 3.
- Builds on the AW runtime spec (`2026-05-22-enterprise-aw-runtime-design.md`, Decision 5) and the manifest tool-refs work.
