# Stage 4 — Component-Manifest-Driven Latency — Design Spec

**Date:** 2026-05-11
**Status:** Draft, pending review
**Related repos:** `greentic-component` (schema + author SDK), `greentic-designer` (`NodeHandler` impls), every extension repo that ships a component (re-publish at 1.2.x), `greentic-runner-host` (consumer)
**Roadmap reference:** `greentic-designer/docs/superpowers/plans/2026-05-10-loading-ux-roadmap.md` — Stage 4
**Sibling spec:** `2026-05-11-stage3-runtime-spinner-design.md`
**Stages 1–3 status:** Stage 1+2 shipped (designer PRs #247, #249, #250). Stage 3 spec landed (runner PR #331).

---

## 1. Context

Stages 1 through 3 of the loading-UX roadmap rely on **designer-side hardcoded knowledge** of which handlers are slow. The relevant call site is `greentic-designer/src/orchestrate/pack_via_packc/handlers/`:

```rust
// http.rs
impl NodeHandler for HttpHandler {
    fn latency_class(&self) -> LatencyClass { LatencyClass::Slow }
    fn requires_loading_ui(&self) -> bool { true }
}

// adaptive_card.rs
impl NodeHandler for AdaptiveCardHandler {
    fn latency_class(&self) -> LatencyClass { LatencyClass::Fast }
}

// llm.rs
impl NodeHandler for LlmHandler {
    fn latency_class(&self) -> LatencyClass { LatencyClass::Slow }
}
```

Two structural costs of this approach:

1. **Designer must know every handler type ahead of time.** Adding a new slow handler (say, a Stable Diffusion call) requires a code change in `greentic-designer` even though the handler ships in a separate extension repo. The designer is supposed to be domain-agnostic (per memory: "Greentic Designer = orchestrator of design, owns no domain logic"). Hardcoding latency_class violates that principle.

2. **No round-trip from component author intent to runtime UX.** Today the component author cannot say "this operation is slow"; they ship a WASM and the designer guesses. The designer's `latency_class()` is its **opinion** of the component, not the component's **own** declaration. If a future component is fast (e.g. an HTTP call to localhost mock), the designer still treats it as Slow because the trait impl says so.

Stage 4 moves latency from designer opinion to **component self-declaration**. Each component manifests its own latency profile; the designer's `NodeHandler::latency_class()` becomes a passthrough that reads from the resolved component manifest at pack-creation time. New slow components self-register without any designer code change.

---

## 2. Goal

A component's `describe()` (a.k.a. `component.manifest.json` per `greentic-component/crates/greentic-component/schemas/v1/component.manifest.schema.json`) declares:

```jsonc
{
  "id": "...",
  "operations": [
    {
      "name": "send",
      "latency_class": "fast" | "slow" | "streaming",
      "expected_p50_ms": 50,
      "expected_p99_ms": 500
    }
  ]
}
```

After Stage 4:

1. Designer's `NodeHandler::latency_class()` reads from the resolved component manifest — **no per-handler hardcoded enum branch**.
2. Adding a new slow component = publish with `latency_class: "slow"` in the manifest; designer picks it up at next pack-create.
3. Stage 3 `loading_steps[]` field in `manifest.cbor` is generated **from** the component manifest, not from `NodeHandler` traits.
4. Existing extensions (AC, http, llm) re-publish with the field populated; behaviour unchanged.
5. `streaming` latency class is now first-class (today it's a TODO in Stage 2).

---

## 3. Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Latency-class location | **Per-operation, inside `component.manifest.json::operations[]`** | A single component can expose `fast` and `slow` operations. Example: `redis-cache.get` (fast) vs `redis-cache.search` (slow). Per-operation granularity matches the existing operations[] surface. |
| Schema additive | New optional field on operation entry. Default `fast`. `additionalProperties: false` of schema needs update. | Strictly additive; existing manifests parse with `latency_class = fast` defaulted. |
| Enum values | `fast` \| `slow` \| `streaming` | Matches designer's existing `LatencyClass` enum exactly. Three is enough; `medium` would just defer the threshold decision to the runner. |
| Expected-latency hints (optional) | `expected_p50_ms: u32`, `expected_p99_ms: u32` on the operation entry. Optional. | Tools can use these for cost/perf dashboards. Not consumed by the runtime emit hook (which is binary fast/slow). |
| `streaming` semantics | Component declares it but does not yet emit progressive output through the host. Stage 4 reserves the enum value; honoring it (e.g. emit `LoadingDone` on first token) lands as a separate task once streaming-export WIT is settled. | Avoids holding Stage 4 hostage to a streaming-WIT design. |
| Designer-side change | `NodeHandler::latency_class()` becomes a passthrough; reads from the resolved component manifest cached in the orchestrator. Trait method stays for compile-time call-site stability, but body collapses to: `self.component_manifest.operations[op_idx].latency_class.unwrap_or(Fast)`. | Smallest disruption to Stage 1/2 code. Trait acts as the seam, manifest is the source. |
| Stage 4 producer of `loading_steps[]` | Designer's `pack_via_packc::mod::add_loading_hints_to_manifest()` (introduced by Stage 3 impl) reads `component_manifest.operations[].latency_class` instead of `NodeHandler::latency_class()`. | Same output shape Stage 3 expects; only the source-of-truth flips. |
| Migration to populated manifests | All Greentic-authored extensions re-published at 1.2.x with the new field. Existing v1.1.x extensions continue to work with `latency_class = fast` defaulted (designer still synthesizes correct hint for known-handler types via a transitional fallback table that maps `kind == http \| llm` → `slow`). | No flag-day. Transitional fallback table deletes once all in-house extensions are on 1.2.x. |
| Schema version bump | `component.manifest.schema.json` version stays at v1 (additive). New optional fields do not require schema major. | Stays within v1 contract. |
| Designer SDK macro | `#[greentic_component]` macro (in `greentic-component`) gains a `latency_class = "slow"` attribute that the proc-macro writes into the generated `describe()` export. | Component authors declare intent at code site; macro emits the manifest field. |
| Validation | `gtdx publish` (greentic-designer-sdk-cli) validates `latency_class` is one of the three values and `expected_pXX_ms` are sane (e.g. p50 < p99). | Catches typos before extensions ship. |

---

## 4. Cross-repo migration matrix

| Phase | Repo | Change |
|-------|------|--------|
| 4.1 | `greentic-component` | Add `latency_class`, `expected_p50_ms`, `expected_p99_ms` fields to `component.manifest.schema.json::operations[]`. Default `latency_class = fast` in the deserializer. Bump component manifest deserializer version field to `1.1`. |
| 4.2 | `greentic-component` | `#[greentic_component]` macro gains optional `latency_class` attribute; emits the field in `describe()`. |
| 4.3 | `greentic-designer-sdk` | `gtdx publish --validate` extends the validator to check `latency_class` enum and `expected_pXX_ms` shape. |
| 4.4 | `greentic-designer` | `pack_via_packc::handlers::{http,llm,adaptive_card}.rs` — `NodeHandler::latency_class()` reads from the orchestrator's resolved component manifest. Keep transitional fallback table for v1.1.x packs (`kind == http \| llm` → slow). |
| 4.5 | `greentic-designer` | `pack_via_packc::mod::add_loading_hints_to_manifest()` (Stage 3 impl, when it lands) reads `component_manifest.operations[].latency_class` instead of trait method. |
| 4.6 | `greentic-adaptive-card-mcp` | Re-publish at 1.2.x with `latency_class: "fast"` on all operations. |
| 4.7 | `greentic-llm-extensions` (per-component repos) | Re-publish at 1.2.x with `latency_class: "slow"` on all operations. |
| 4.8 | `greentic-provider-extensions` (http, kv-store, …) | Re-publish at 1.2.x with appropriate `latency_class` per operation. |
| 4.9 | `greentic-runner-host` | Optional: log `latency_class` of crossed nodes at TRACE level for observability. No correctness dependency on Stage 4. |
| 4.10 | Roll out | Once all in-house 1.2.x extensions are published, delete the transitional fallback table in designer. v1.1.x packs still build because `latency_class = fast` defaults. |

---

## 5. Architecture

### 5.1 Source of truth flow (after Stage 4)

```
Component author writes:
    #[greentic_operation(latency_class = "slow", expected_p50_ms = 800)]
    async fn search(...) { ... }

build.rs / SDK proc-macro emits describe() output:
    {
      "operations": [
        { "name": "search", "latency_class": "slow", "expected_p50_ms": 800, ... }
      ]
    }

   ↓ ships as .gtxpack

Designer's pack_via_packc loads referenced components on pack-create:
    component_manifests: HashMap<ComponentId, ComponentManifest>

Designer's NodeHandler::latency_class() impl:
    fn latency_class(&self) -> LatencyClass {
        self.ctx.component_manifests
            .get(&self.component_id)
            .and_then(|m| m.operation(&self.op_name))
            .map(|op| op.latency_class)
            .unwrap_or(LatencyClass::Fast)
    }

   ↓ designer computes slow_to_loading map (Stage 2) and loading_steps[] (Stage 3)

Pack ships with loading_steps[] populated correctly.

Runner reads loading_steps[] (Stage 3 mechanism).
Webchat embed renders spinner (Stage 3 mechanism).
```

### 5.2 What disappears

- The hardcoded `LatencyClass::Slow` literal in `greentic-designer/src/orchestrate/pack_via_packc/handlers/{http,llm}.rs`.
- The need for designer to ship a new release every time a new slow handler is invented elsewhere.
- The mismatch between "designer thinks this is slow" and "component author knows their localhost mock is fast".

### 5.3 What stays

- `NodeHandler` trait — still the seam between orchestrator and handlers; just becomes thinner.
- `LatencyClass` enum in designer — same three variants; still used by orchestrator internally.
- Stage 1 condition walker; Stage 2 auto-card injection; Stage 3 manifest hints / runtime emit — all unchanged, they just consume the new source.

---

## 6. Out of scope

- **Honoring `streaming` end-to-end** — Stage 4 reserves the enum value but does not change runner / webchat behaviour for streaming. That's a follow-up once streaming-export WIT is settled.
- **Cost / SLA dashboards from `expected_pXX_ms`** — purely informational on the manifest; consumers TBD.
- **Adaptive latency classification** — runner-side measurement + auto-reclassification of "looked fast but is actually slow" operations. Different problem, different spec.
- **Designer UI surface** — showing the operator "this node is slow" in the canvas Inspector — possible follow-up; Stage 4 only changes the data, not the UX.
- **Operator-flow YAML annotation** — flow authors cannot override `latency_class` per-node. Component author's manifest is authoritative.

---

## 7. Open questions

1. **Per-operation vs per-component granularity** — proposal is per-operation. Alternative: per-component, simpler but loses precision (a kv-store component has both fast `get` and slow `search`). Inclination: stick with per-operation; the schema already supports operations[].

2. **`gtdx publish` strictness** — should `latency_class` become **required** on operations[] after a soak period, or stay optional with `fast` default forever? Recommendation: stay optional; required would force a re-publish for every existing community extension on every breaking schema change.

3. **Transitional fallback table lifetime** — proposal is "delete once all in-house extensions are on 1.2.x". When is that? Need a checklist. Suggest: track in the loading-ux-roadmap as a follow-up item, gate the deletion on the checklist completing.

4. **Designer SDK macro attribute name** — `latency_class = "slow"` vs `slow = true` vs `latency = "slow"`. Inclination: `latency_class` for symmetry with the manifest field.

5. **Streaming-aware operations** — proposal reserves `streaming` but doesn't honor it. Should the manifest field be `latency_class: ["slow", "streaming"]` (multi-tag) or single-value? Single-value seems cleaner; streaming is itself a kind of slow, and the consumer (designer + runtime) can interpret it appropriately when streaming-WIT lands.

6. **Validation strictness for `expected_pXX_ms`** — should `gtdx publish` reject manifests where p50 ≥ p99? Probably yes (cheap correctness check), but defer until 4.3 lands.

---

## 8. Testing plan

| Layer | Test | Repo |
|-------|------|------|
| Schema | Unit: `component.manifest.schema.json` validates manifests with and without `latency_class`; defaults to `fast`. | `greentic-component` |
| Schema | Unit: validator rejects `latency_class: "medium"` (not in enum) and `expected_p50_ms > expected_p99_ms`. | `greentic-component` |
| SDK macro | Unit: `#[greentic_operation(latency_class = "slow")]` emits manifest field. | `greentic-component` |
| Designer | Unit: orchestrator resolves `NodeHandler::latency_class()` via component-manifest lookup; matches manifest. | `greentic-designer` |
| Designer | Unit: transitional fallback fires when component manifest lacks `latency_class` field (v1.1.x pack). | `greentic-designer` |
| Designer | Integration: pack with mixed-latency components produces correct `slow_to_loading` map and `loading_steps[]` exactly equivalent to today's hardcoded output. | `greentic-designer` |
| Cross-repo | Conformance test in `greentic-runner` consumes a designer-built 1.2.x pack with slow components; manifest hint is populated; runner emits LoadingStart per Stage 3. | `greentic-runner` |
| Extension re-publish | Per-extension PR adds the field; existing tests still pass. | `greentic-adaptive-card-mcp`, `greentic-llm-extensions`, … |

---

## 9. Rejected alternatives

- **Designer-side latency database** — a registry mapping component-id → latency. Effectively what we have today plus indirection. Doesn't solve "designer doesn't know about your new component".
- **Runtime auto-classification** — runner measures and reclassifies. Different problem; doesn't replace authoring intent. Could complement Stage 4 later.
- **Per-flow-node latency override** in YAML — moves the decision from author to flow designer. Wrong place. Component author knows their own latency profile; flow designer doesn't.
- **WIT-export field instead of manifest** — `describe()` is a WIT export today (per `greentic-component-runtime/src/loader.rs`), but the **manifest** is the curated subset that operators consume. Putting `latency_class` in WIT would force a v0.7 component world. Manifest is the right surface — it's already the "structured `describe()`".

---

## 10. Sequencing

Stage 4 has **no hard dependency on Stage 3 landing first** — they're independent improvements. But:

- If Stage 3 lands first, Stage 4 is "easy": just flip the source from `NodeHandler::latency_class()` to manifest lookup.
- If Stage 4 lands first (e.g. because Stage 3 stalls on webchat-embed coordination), Stage 3's `loading_steps[]` builder still works — it just continues to ask the trait method, which now reads from manifest. No change to Stage 3 impl.

**Recommended order:** Stage 4 schema + designer changes first (4.1 – 4.5), in-house extension re-publish in parallel (4.6 – 4.8), then delete the transitional fallback table (4.10) once the in-house extensions are on 1.2.x. Stage 3 implementation proceeds independently against either old or new source.

---

## 11. Review checklist

- [ ] Bima sign-off on per-operation vs per-component granularity
- [ ] Confirm component-author SDK macro lands in `greentic-component` (vs `greentic-designer-sdk`)
- [ ] Sequence with Stage 3 spec — both can land independently, but document the order we'll actually execute
- [ ] Transitional fallback table lifetime — agree on the in-house re-publish checklist
- [ ] `streaming` value — reserve only, or also wire up to Stage-2 fallback card injection now?
