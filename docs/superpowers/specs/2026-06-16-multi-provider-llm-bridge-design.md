# Multi-provider LLM for digital workers — design

**Date:** 2026-06-16
**Status:** Approved (design); sub-project 1 ready for planning
**Scope owner repo:** `greentic-runner` (runtime is the integration point); spans `greentic-llm-extensions`, `greentic-dw`, `greentic-secrets`, `greentic-start`.

## Problem

The designer's digital-worker form shows an **LLM Model** dropdown, but it only offers
"OpenAI Chat" / "OpenAI Chat Lite". Investigation showed the limitation is real at **three
independent layers**, and only the last one is what makes a chosen provider actually *work*:

| Layer | Why only OpenAI today | Fixed by |
| --- | --- | --- |
| Dropdown UI | `assets/dw-providers-catalog.json` (vendored, embedded via `include_str!`) has only two `cap://llm/chat` entries, both OpenAI. | Catalog entries |
| Artifact | Those entries are **example/seed data** (`greentic-dw/examples/providers/catalog.json`) pointing at `oci://ghcr.io/greenticai/packs/providers/llm/openai-chat:latest` which **does not exist** in GHCR (verified: `oras manifest fetch` → not found, while a real pack `packs/dw/memory/short-term-redis-pack:latest` resolves). | Catalog entries |
| **Runtime execution** | The agentic-worker runtime (`greentic-aw-runtime`) builds **one global** `Arc<dyn LlmBackend>` chosen by **env vars** in `build_agent_node_handler` (`greentic-runner-host/src/runner/agent_node.rs:341-389`): default in-process `OpenAiLlmBackend` (hardcoded OpenAI), or `ExtensionLlmBackend` if `GREENTIC_AW_LLM_EXTENSION` is set. The per-worker `provider_id` rides in `LlmRequest.provider` (`loop.rs:81-85`) but **both backends ignore it** for routing. The runner does **not** depend on `greentic-llm` and does **not** load any LLM provider from a `.gtpack`. | This design |

**Key correction to the original framing:** releasing the `greentic-dw-providers` LLM crates as
`.gtpack` artifacts does **not** make any provider runnable — the runner never loads LLM logic
from a pack. Those packs (and catalog entries) only populate the designer's dropdown + wizard
config. Runtime execution is a separate seam.

## Goal

A digital worker's chosen LLM provider (from the dropdown) executes **end-to-end at runtime**,
**per-tenant**, for the full `greentic-llm` provider set (9: openai, anthropic, deepseek, gemini,
cohere, ollama, groq, perplexity, xai), via a **WASM LLM-bridge extension**, with API keys
resolved **per-tenant** from the shared secrets broker.

## Chosen architecture — Arch A: LLM-as-extension

The runner already contains the seam for this ("Approach B" in the code comments):
`ExtensionLlmBackend` (`greentic-aw-runtime/src/llm_extension.rs`) invokes an extension tool
named `complete` with a `BridgeRequest { system_prompt, history, tools, credential }` and parses
back an `LlmResponse`. This code is built and unit-tested with mocks, but **no real bridge
extension exists** and the **credential is frozen from env at construction**, not resolved
per request.

### Why not Arch B (in-process `greentic-llm`)

`greentic-llm` **cannot compile to `wasm32-wasip2`** (async + `rig-core` + reqwest/native TLS),
so it cannot live inside an extension. It *could* be compiled into the runner binary as a routing
backend, but that puts every provider in the host binary (against the extension model) and still
needs catalog work. Arch A keeps providers out of the binary and matches the existing seam. The
trade-off accepted: the bridge must **hand-roll per-provider HTTP** (sync, via the host
`greentic:extension-host/http@0.1.0` `fetch` import). This is a proven pattern — see
`greentic-llm-extensions/reference-extensions/llm-openai/src/openai_client.rs` +
`test_prompt.rs` (it calls `host_http::fetch` synchronously, no async/TLS in the guest).

## Target data flow

```
Designer form (pick "Anthropic" + model + credential-ref)
  → AgentConfig.llm { provider:"anthropic", model:"claude-…" }   (embedded in worker config)
  → runner loop builds LlmRequest{ provider=anthropic }          (loop.rs:81-85)
  → ExtensionLlmBackend RESOLVES credential per-tenant:
        SecretsManager.read(secrets://default/{tenant}/_/llm/anthropic)   → api_key
        BridgeCredential { provider:"anthropic", model, api_key, base_url? }
  → ext_runtime.invoke_tool(bridge_id, "complete", BridgeRequest{…, credential})
  → bridge routes on credential.provider → anthropic mapper
  → host_http::fetch → https://api.anthropic.com/v1/messages
  → parse → LlmResponse { content, tool_calls[], tokens_in, tokens_out }
  → back to the Plan-Act-Observe loop
```

## Components

### 1. `greentic-llm-bridge` extension (new, in `greentic-llm-extensions`)

- WASM component, `wasm32-wasip2`, `cargo component`. Template: `reference-extensions/llm-openai`.
- Exposes a runtime tool **`complete`** whose args/result match the runner's existing
  `BridgeRequest`/`LlmResponse` wire shape (`llm_extension.rs:28-44`).
- Routes internally on `credential.provider` to one hand-rolled mapper per provider. Each mapper:
  builds the provider URL + auth header + request body, calls `host_http::fetch`, parses
  content + tool_calls + usage. Default base URLs per provider, overridable by
  `credential.base_url`.
- Tool-calling: translate `LlmToolSchema[]` to each provider's function/tool format and parse
  tool calls back into `ToolCallRecord[]`. Providers with limited/absent tool support
  (cohere/perplexity/ollama variants) degrade gracefully (no tool_calls), never panic.
- `describe.json`: network permission allowlist for the provider endpoints. **No host secrets
  import** — the runner pre-resolves and passes the credential in the args.
- Build/publish: `cargo component build --release --target wasm32-wasip2` → `build.sh` →
  `.gtxpack` → Store via `greenticai/greentic-designer-extension-action@v2` + git tag.

**Open contract detail (resolve in plan step 1):** confirm which WIT tools interface
`greentic-ext-runtime::ExtensionRuntime::invoke_tool` dispatches at runtime, and export `complete`
through exactly that interface (the existing `llm-openai` exposes *design-time*
`greentic:extension-design/tools`; the bridge needs the runtime-invocable surface).

### 2. `greentic-runner` — per-request, per-tenant credential resolution

- **Broker secrets client** (new): implement `greentic_secrets_lib::SecretsManager` over the
  `greentic-secrets-broker` HTTP API (`GET /v1/{env}/{tenant}/{category}/{name}`), add a
  `SecretsBackend::Broker { endpoint, token }` variant + config
  (`SECRETS_BACKEND=broker`, `SECRETS_BROKER_ENDPOINT`, `SECRETS_BROKER_TOKEN`). Wrap with the
  existing `CachingSecretsManager`. (~400 LoC + tests.) Files:
  `greentic-runner-host/src/secrets.rs`.
- **Credential resolution**: change `ExtensionLlmBackend` from a frozen `credential` to resolving
  a `BridgeCredential` **per `complete` call** from `request.provider.provider` +
  `request.provider.model` + the tenant-scoped secrets manager. Thread `SecretsManager` +
  `TenantContext` into `AgentRuntime` / the backend (today `build_agent_node_handler` is per-tenant
  and the secrets manager is per-pack; the LLM credential is **tenant-wide**, so it does not need
  pack scope). The runner's secret URI for LLM must match what admin writes:
  `secrets://default/{tenant}/_/llm/{provider}` — **not** the pack-scoped
  `secrets://env/tenant/team/pack/key` helper.
- Default `GREENTIC_AW_LLM_EXTENSION` to the published bridge id in deploy config.

**RESOLVED (2026-06-16) — identifier chain.** Admin stores the LLM key at
`secrets://default/{tenant}/_/llm/{provider_id}` where `provider_id` is the **admin
`tenant_llm_providers.id` UUID**, not a slug. The runner only has the provider *slug* + model.
Decision: **carry the credential ref in the worker config.** Extend `LlmProviderRef` with
`credential_ref: Option<String>` (= the admin provider UUID); the designer
`dw_form_to_agent_config` populates it from the form's selected credential; the runner reads
`secrets://default/{tenant}/_/llm/{credential_ref}`. `provider` (slug) still drives bridge
routing; `model` still comes from `request.provider.model`. This is multi-tenant-safe and
supports multiple credentials per brand. Adds a coordinated change to `greentic-designer`
(form→config mapping) and the shared `LlmProviderRef` type.

**Provider mapper families (simplification).** The 9 providers collapse to **4 mapper shapes**:
OpenAI-style `/v1/chat/completions` + Bearer (openai, deepseek, groq, perplexity, xai, ollama,
openai-compatible — one parameterized mapper by base_url/model), Anthropic `/v1/messages`,
Gemini `:generateContent`, Cohere `/v2/chat`.

### 3. Designer catalog (in `greentic-dw` → designer assets)

- Add provider entries (`cap://llm/chat`) to `greentic-dw/examples/providers/catalog.json` with
  `display_name`, `brand`, model select, and a credential-ref question block; run
  `greentic-designer/scripts/refresh-dw-catalog.sh` to sync into
  `greentic-designer/assets/dw-providers-catalog.json`.
- Map each `provider_id` to the canonical bridge provider string.
- `greentic-dw-providers` `.gtpack` release stays **optional** (wizard-QA reuse only); **not** on
  the runtime critical path.

### 4. Per-tenant credential provisioning (foundation — already mostly built)

- `greentic-secrets-broker` is a shared networked secrets service. **Admin already writes**
  per-tenant LLM keys to it today (dual-write, live):
  `greentic-designer-admin/src/routes/admin/tenant_llm.rs` →
  `s.secrets.put_secret(scope{env:"default",tenant,team:None}, Llm, provider_id, api_key)` →
  URI `secrets://default/{tenant}/_/llm/{provider_id}`. Designer reads facade-first.
- Therefore the runner only needs to **read** the broker (component 2). No new write path.
- Caveats: `env` is `default` in the current cutover (runtime `TenantCtx` carries no env yet);
  team is tenant-wide `_`; cloud KMS backend selection is deferred (local FileBackend default).

### 5. Deploy (`greentic-start` / ECS)

- Set `GREENTIC_AW_LLM_EXTENSION=<bridge id>` and broker config
  (`SECRETS_BACKEND=broker`, endpoint, token) on the runner task. Per-provider env keys are no
  longer the credential source (Option B).

## Decomposition into sub-projects

Per decision (2026-06-16): build the **full 9-provider set in one plan** (sub-projects 1+2
merged). Plan **sub-project 1** next; sub-project 3 is an optional follow-up.

### Sub-project 1 — full end-to-end, all 9 providers
Proves the entire chain AND ships every provider.
1. Bridge extension with **all 9** provider mappers (openai, anthropic, deepseek, gemini, cohere,
   ollama, groq, perplexity, xai) + `complete` tool; build + publish `.gtxpack`. Build the chain
   on OpenAI + Anthropic first (the two best-understood mappers) to validate the wire contract,
   then add the remaining seven mappers behind the same `complete` interface — internal sequencing
   only, single deliverable.
2. Runner broker `SecretsManager` client + `SecretsBackend::Broker` + config.
3. Runner per-request credential resolution in `ExtensionLlmBackend`; thread
   SecretsManager + TenantContext; default the bridge env.
4. **Nine** catalog entries → refresh into designer assets; pin provider-string mapping.
5. Deploy env + **live smoke**: at least one provider (Anthropic) answering through a deployed
   operator; per-provider unit fixtures cover the other eight.

### Sub-project 3 — productionize per-tenant credentials (optional follow-up)
Admin UI coverage for per-tenant LLM credential management (verify against Slice C) + cloud KMS
backend wiring for the broker in production.

## Testing strategy

- **Bridge**: per-provider unit tests with recorded request/response fixtures over a mock
  `host_http`; `wasm-tools validate` on the built component.
- **Runner**: `SecretsManager` broker-client tests (mock broker); `ExtensionLlmBackend`
  per-request resolution test (mock invoker + mock secrets, asserting the credential is built from
  `request.provider`, not a frozen value).
- **E2E**: one provider (Anthropic) live smoke through a deployed operator reading a real
  per-tenant key from the broker.

## Risks / open items

1. **Runtime tools interface for the bridge** (`complete` export surface) — verify first.
2. **Identifier reconciliation** catalog `provider_id` ↔ `AgentConfig.llm.provider` ↔ broker key.
3. **Secret URI shape** for LLM (`default`/`_`) vs the runner's pack-scoped helper — must align
   with admin's writes.
4. **Tool-calling parity** across providers — degrade gracefully where unsupported.
5. **Broker availability/auth** from the runner (token provisioning, healthz gating) in deploy.
