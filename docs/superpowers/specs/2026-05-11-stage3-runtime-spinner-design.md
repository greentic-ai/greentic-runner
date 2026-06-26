# Stage 3 — Runtime-side Loading Spinner — Design Spec

**Date:** 2026-05-11
**Status:** Draft, pending review
**Related repos:** `greentic-runner` (event emit), `greentic-start` (WS relay), webchat client (external; render only)
**Roadmap reference:** `greentic-designer/docs/superpowers/plans/2026-05-10-loading-ux-roadmap.md` — Stage 3
**Stages 1-2 status:** shipped (designer PRs #247, #249, #250)

---

## 1. Context

Stages 1 and 2 of the loading-UX roadmap shipped end-to-end in `greentic-designer`:

| Stage | Mechanism | Status |
|-------|-----------|--------|
| 1 | `NodeHandler::latency_class()` trait method + `slow_to_loading` orchestrator map | Done (PR #247 walker, #249 trait) |
| 2 | Auto-inject synthetic loading card into pack between trigger and slow handler; surfaces `auto_loading_cards` opt-in on `POST /api/wizard/build` | Done (PR #249 + #250) |
| 3 | **Runtime emits Loading / LoadingDone events; webchat renders inline spinner** | **This spec** |
| 4 | Component `describe()` declares `latency_class`; trait method becomes a passthrough | Future |

Stage 2 produces a working UX today by polluting the flow YAML with synthetic cards. That is acceptable for v1.0 but has three known costs:

1. **Flow YAML pollution** — every slow leg gets a sibling `auto_loading_*` card written into `flows/main.ygtc`. Surfaces in operator's flow trace, in any "view source" UI, and in YAML diffs.
2. **No streaming-handler story** — Stage 2 fires the loading card *once* before the slow leg. Streaming responses (LLM token-by-token) cannot reuse the same UX: the runner would have to delete the loading card mid-flow, which the canvas doesn't model.
3. **Cross-channel inconsistency** — only webchat-style packs benefit. WhatsApp / Telegram / Slack ignore the synthetic card entirely (they don't render it pre-emptively).

Stage 3 moves the loading UX *out of the flow data plane* and into the runtime → transport → client control plane. The synthetic card stays as a fallback for surfaces that lack a structured loading event.

---

## 2. Goal

When the runner crosses a node flagged "slow", emit a typed `Loading` event before invocation and a `LoadingDone` event after. The webchat-side client renders these as an inline spinner overlaid on the last bot reply (or as a "typing" indicator above the input box, depending on `display_style`).

Acceptance:

1. WebChat client visually shows "Working on it…" within 100 ms of the runner entering a slow node.
2. Spinner clears within 100 ms of the slow node returning (success or error).
3. Polling-fallback clients (`GET /v3/directline/.../activities`) see the same events as activity-shaped frames (graceful degradation).
4. Non-webchat surfaces (WhatsApp, Telegram, ...) silently ignore the events with zero error.
5. Auto-loading card (Stage 2) becomes opt-out, defaulting **off** when the deploy target advertises Stage-3 capability.

---

## 3. Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Latency source at runtime | **Pack manifest declares `loading_steps: [step_id, …]`** at build time; runner reads on pack load. | Stage 4 (`describe()` manifest) is the clean answer but is separately scoped. Designer already computes `slow_to_loading` map in `pack_via_packc::mod` (Stage 2) — that same set lands in the manifest. Zero round-trip cost; runner has no opinion about component internals. Migration to Stage 4 = replace the source of truth, transport unchanged. |
| Event transport | **Direct Line custom activity subtype** `{"type":"event","name":"greentic.loading.start","value":{"step_id":"…","label":"…"}}` and `greentic.loading.done` | Reuses existing ActivitySet frame plumbing in `greentic-start/src/http_ingress/websocket/`. Microsoft `botframework-webchat` exposes `EventActivity` to middleware — clients can subscribe without protocol changes. Custom name keeps us out of the reserved `typing` / `messageReaction` namespace. |
| Why not DirectLine `typing` activity | `typing` is a single-shot indicator with no payload + no done-marker; webchat lib auto-clears it after 5 s. Insufficient for slow legs that may exceed 5 s. | |
| Emit point (runner) | `crates/greentic-runner-host/src/runner/operator.rs::invoke` (line 1037) wraps invocation with `before_slow_node` / `after_slow_node` hooks driven off a pre-resolved `loading_set: HashSet<NodeId>` on the loaded `PackRuntime`. | Sole entry point through which every handler invocation funnels. Already async, already trace-instrumented. |
| Emit-side serialization | `RuntimeEvent::LoadingStart { step_id, label }` → outbound activity via existing `Channel::Outbound` queue in engine. No new channel. | The webchat outbound pump already drains this queue; the new variant arrives at `greentic-start/src/http_ingress/websocket/pump.rs` as an additional `Value` in the next ActivitySet. |
| Label resolution | Label comes from the **loading card** the designer would have synthesized in Stage 2 — same i18n key, same `${var}` template binding. Designer emits this verbatim into `loading_steps[]` entries. | Single source of truth; client-side renders user-facing copy already authored by the flow designer; no new i18n catalogue. |
| Multi-step / streaming | Step IDs are unique per node. Streaming handlers emit `LoadingStart` once at entry; emit `LoadingDone` on **first token streamed back** (not on full response). | Matches user perception of "the bot is responding" vs "the bot is thinking". Implementation note: streaming handlers must call a host-side `tokio::sync::oneshot` from inside `invoke` before yielding their first chunk. |
| Backward compatibility | Runner without manifest field → empty `loading_set` → no events. Pack without field → same. Client without subscriber → events show up in middleware as no-op. | Zero forced version bump in any direction. |
| Pack manifest field | New top-level `loading_steps: Vec<LoadingStepHint>` in `manifest.cbor`, additive, optional. | CBOR additive change. `LoadingStepHint { step_id: String, label: I18nString, display_style: "spinner" | "typing" | "dim_card" }`. |
| Designer's Stage 2 fallback | Stage 2 auto-card stays. New flag on `WizardBuildBody`: `runtime_loading_capable: bool` (default false during rollout, flip to default-true once `greentic-start` is on `v0.X+`). When `true`, designer **emits manifest hint only**; skips synthetic card injection. | Lets us roll Stage 3 out without forcing a re-pack of every existing flow. |

---

## 4. Architecture

### 4.1 Layer placement

```
greentic-designer (Stage 2 + 3 producer)
   pack_via_packc::mod::add_loading_hints_to_manifest()
       reuses slow_to_loading map already built for Stage 2
       writes loading_steps[] into manifest.cbor

       ↓ .gtpack on disk

greentic-runner-host (Stage 3 emit)
   pack.rs::load_manifest()
       parses loading_steps[] into PackRuntime.loading_set: HashMap<NodeId, LoadingStepHint>
   runner/operator.rs::invoke()
       if loading_set.contains(&node_id) {
           outbound.push(make_loading_start_activity(&hint));
           let result = invoke_inner(...).await;
           outbound.push(make_loading_done_activity(&hint));
           result
       }

       ↓ ActivitySet over WS

greentic-start/src/http_ingress/websocket (Stage 3 transport)
   pump.rs::Outbound
       (no change — already JSON-passthrough)

       ↓ Direct Line frame over WebSocket

webchat client (external; render only)
   Middleware subscribes to event activity name "greentic.loading.start"
       renders inline spinner with hint.label
   Middleware subscribes to event activity name "greentic.loading.done"
       removes spinner
```

### 4.2 Data flow per slow-node hop

```
T+0    Client: user clicks AC submit → POST /v3/directline/.../activities
T+0    greentic-start → forwards to greentic-runner-host via internal RPC
T+1ms  runner engine: enters slow node, looks up loading_set, found
T+1ms  runner → outbound queue: { type: "event", name: "greentic.loading.start",
                                  value: { step_id: "http_lookup", label: "Checking availability…" } }
T+2ms  ws pump drains queue → ActivitySet frame on WS
T+5ms  client middleware: render spinner with "Checking availability…"
...    HTTP handler runs (e.g. 3 seconds)
T+3s   runner engine: slow node returns
T+3s   runner → outbound queue: { type: "event", name: "greentic.loading.done", value: { step_id: "http_lookup" } }
T+3s+5ms client middleware: clear spinner
T+3s+5ms client renders next bot reply (if any) normally
```

### 4.3 Failure modes

| Failure | Handling |
|---------|----------|
| Slow node panics / errors | `LoadingDone` is emitted via `Drop` on a guard struct wrapping the invocation future. Guarantees spinner is never left hanging. |
| WS disconnected mid-hop | Client reconnects via DirectLine `streamUrl`, polls with `?watermark=N` to catch up. Events are in the activity queue with watermarks, so they replay deterministically. |
| Streaming handler never yields | Spinner stays up until the engine-level invocation timeout fires (default 30 s); timeout path emits `LoadingDone` before propagating the error activity. |
| Client doesn't subscribe to events | Webchat lib renders event activities as no-op (default behavior); no visual artefact. |
| Non-webchat channel | Provider adapter (Telegram / Slack / WhatsApp) filters events out of `ActivitySet` before relay — done in the per-provider `adapt_*.rs` modules. |

---

## 5. Out of scope

- **Stage 4 component-manifest-driven latency** — separate spec (`docs/superpowers/specs/TBD-stage4-describe-latency.md`). Once Stage 4 lands, the `loading_steps[]` manifest field becomes derivable from component `describe()` output instead of designer-side hardcoded handlers; transport and rendering are unchanged.
- **Cross-tenant aggregation** — no global "slow operations dashboard" in this spec. That belongs in `greentic-telemetry`.
- **i18n / localization of `label`** — uses existing greentic-i18n at flow-author time. Spec does not add a new catalogue.
- **WhatsApp / Telegram / Slack rendering** — explicitly drop. These providers can lift the same data later from the activity stream, but is not required for Stage 3 acceptance.
- **WebChat client UX details** — colour, animation timing, position. Decided by webchat embed maintainer, out of platform scope.

---

## 6. Migration plan

| Phase | Repo | Change |
|-------|------|--------|
| 3.1 | `greentic-runner-host` | Parse `manifest.cbor.loading_steps[]`. Empty by default; back-compat with existing packs. Wrap `invoke()` with emit hooks. |
| 3.2 | `greentic-runner-host` | New `RuntimeEvent::Loading{Start,Done}` activity-builder helpers in `runner/operator.rs`. Drop-guard for failure path. |
| 3.3 | `greentic-start` | None required at WS layer (passthrough). Optional: add metric counter for forwarded loading events. |
| 3.4 | `greentic-designer` (`pack_via_packc::mod`) | Emit `loading_steps[]` into manifest builder. Adds `WizardBuildBody.runtime_loading_capable: bool`. When set, skip Stage-2 synthetic card injection. |
| 3.5 | WebChat embed (external) | Subscribe middleware on `greentic.loading.start` / `greentic.loading.done`. Render spinner per `display_style`. |
| 3.6 | `greentic-runner-host` integration test | E2E: pack with one slow node → runner → mock-WS → assert event sequence. |
| 3.7 | Roll out | Default `runtime_loading_capable=false` for one release. Flip to default-true once webchat embed ships subscriber. |

---

## 7. Open questions

1. **Manifest field placement** — should `loading_steps[]` live at top-level of `manifest.cbor` (alongside `flows[]`, `components[]`) or nested under each flow entry? Inclination: top-level, since runner indexes by `node_id` which is already flow-scoped.

2. **Streaming-handler hint shape** — the `display_style` enum currently has `spinner`, `typing`, `dim_card`. Is "streaming" itself a fourth style (typewriter cursor render) or just a hint-emission-timing variant? Defer until first streaming handler example is built.

3. **Stage 4 dovetail** — when Stage 4 lands, does designer continue computing `loading_steps[]` (transitional double-source) or does runner read directly from component `describe()` and ignore the manifest field? Recommendation: keep the manifest field forever as the runtime-time source of truth; let Stage 4 just feed it.

4. **WhatsApp / Telegram catch-up** — non-webchat providers could, in theory, emit a "typing" indicator at the channel level when the runner crosses a slow node. Not in scope for Stage 3 acceptance, but worth a follow-up issue.

5. **Per-tenant override** — should a tenant be able to opt out of Stage 3 emission (e.g. for ultra-latency-sensitive deployments where the extra event traffic matters)? Recommendation: yes, via a tenant-config boolean, default on.

---

## 8. Testing plan

| Layer | Test | Repo |
|-------|------|------|
| Designer | Unit: `pack_via_packc::mod` emits one `loading_steps[]` entry per slow handler in `slow_to_loading`. Snapshot test on the produced manifest CBOR. | `greentic-designer` |
| Designer | Integration: `runtime_loading_capable=true` → no `auto_loading_*` card in flow YAML; `false` → card present (Stage 2 fallback). | `greentic-designer` |
| Runner | Unit: `manifest::parse_loading_steps()` round-trips with `cbor` write/read. | `greentic-runner-host` |
| Runner | Integration: synthetic pack with one slow handler; assert outbound queue receives `LoadingStart` before invocation, `LoadingDone` after; assert `Drop` guard emits done on panic. | `greentic-runner-host` |
| Runner | Integration: streaming handler that yields after 200 ms; assert `LoadingDone` fires on first token, not last. | `greentic-runner-host` |
| Start | Pump test: outbound `Value` with `type=event,name=greentic.loading.start` serialises into ActivitySet frame correctly; watermark increments. | `greentic-start` |
| E2E | `greentic-e2e` adds a WebChat scenario: bot does an HTTP request that takes 3 s; assert client (puppeteer) sees spinner element for ~3 s. | `greentic-e2e` |

---

## 9. Rejected alternatives

- **DirectLine `typing` activity** — single-shot, no payload, auto-clears at 5 s. Cannot represent slow legs > 5 s or carry a label.
- **Server-Sent Events sidecar** — would need a second long-lived connection per session. WS already does the job.
- **Inline loading text in normal bot reply** — same as Stage 2 with extra steps; loses the "outside the data plane" win.
- **Runner emits via stdout tracing → client tails logs** — security and infra non-starter.
- **Make Stage 3 wait for Stage 4** — explicit roadmap recommendation, but Stage 4 is cross-extension-repo and unlikely to complete in the v1.0 cycle. Stage 3 via manifest hint is the v1.0-shippable subset; Stage 4 dovetails later.

---

## 10. Review checklist

- [ ] Bima / Maarten sign-off on transport choice (Direct Line custom event activity)
- [ ] Confirm webchat embed maintainer can ship subscriber middleware
- [ ] Confirm `manifest.cbor` additive-CBOR change is acceptable to operator team
- [ ] Sequence with Stage 4 design lead — does manifest field stay as runtime source of truth?
- [ ] Tenant-config opt-out: in scope for first impl or follow-up?
