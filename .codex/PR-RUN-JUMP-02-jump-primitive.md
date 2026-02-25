# PR-RUN-JUMP-02: Add runner-managed Jump primitive (platform control) — revised
Date: 2026-02-25
Repo: greentic-runner (crates/greentic-runner-host)

## Objective
Implement **component-proposed jump** as a runner-owned primitive with minimal surface change.

Requirements:
- Keep existing flow behavior unchanged when no jump control marker is present.
- Validate and apply jumps in one place.
- Persist redirect intent safely for resume.
- Avoid accidental control triggers from regular payload data.

## v1 Scope
- Jump/replace only (no call/return stack).
- Same pack only.
- Target: `flow` required, `node` optional.
- Loop guard in durable execution state (default max redirects = `3`).
- Control marker must be explicit and namespaced.

## Control contract (v1)
Runner recognizes jump only when component output contains:

```json
{
  "greentic_control": {
    "action": "jump",
    "v": 1,
    "flow": "target.flow",
    "node": "optional-node",
    "payload": {},
    "hints": {},
    "max_redirects": 3,
    "reason": "optional"
  }
}
```

Notes:
- Do not infer jump from plain `flow`/`node` fields.
- Keep JSON compatibility now.
- Document CBOR envelope as future transport extension, not part of v1 implementation.

## Implementation plan (surgical)

### 1) Add typed control channel in runner engine
In `crates/greentic-runner-host/src/runner/engine.rs`, replace ad hoc `wait_reason` signaling with a small control enum returned by node dispatch:

- `Continue`
- `Wait { reason: Option<String> }`
- `Jump { flow: String, node: Option<String>, payload: Value, hints: Value, max_redirects: Option<u32>, reason: Option<String> }`
- `Respond { text: Option<String>, card_cbor: Option<Vec<u8>>, needs_user: Option<bool> }` (reserved now, can be minimally wired)

`Respond` is included now to avoid a second control-channel shape change later.

### 2) Parse jump intent from component output (backward-compatible)
In the component return handling path, add parser logic that:
- checks for `greentic_control.action == "jump"` and `v == 1`
- constructs `NodeControl::Jump` only for that marker
- otherwise returns regular `Continue` behavior with existing payload semantics

### 3) Apply jump in engine loop
In the single engine loop control match, add `Jump` handling via `apply_jump(...)`.

`apply_jump(...)` responsibilities:
1. Resolve target flow from jump payload (required).
2. Resolve target node:
   - use provided node if set
   - else use existing runtime semantics: `entrypoints["default"]`, else first node in flow map.
3. Validate target flow and node using loaded `HostFlow`.
4. Enforce loop guard from durable state counter:
   - read current redirect count from `ExecutionState`
   - max = jump override or default `3`
   - fail when count >= max
   - increment and persist in state on success
5. Apply payload/hints handoff to the next node state (without breaking existing non-jump paths).

### 4) Snapshot/resume updates
Extend `FlowSnapshot` with:
- `next_flow: Option<String>` (flow id in the same identifier space used by `get_or_load_flow`)

Compatibility:
- old snapshots deserialize with `next_flow = None`.

Resume rules:
- if snapshot contains `next_flow`, resume that flow regardless of inbound envelope flow id.
- keep existing tenant/session safety checks.
- keep pack consistency checks for same-pack scope.

### 5) Error handling
On jump validation/apply failure:
- do not partially mutate cursor/snapshot
- return through existing error path
- log stable codes: `missing_flow`, `unknown_flow`, `unknown_node`, `redirect_limit`, `jump_failed`

### 6) Tests
Add tests covering:
1. Marker gating:
   - payload with `flow/node` but no `greentic_control` marker does not jump
2. `apply_jump` unit cases:
   - `unknown_flow`
   - `unknown_node`
   - default node resolution uses current runtime semantics
   - redirect limit enforcement
3. Resume behavior:
   - snapshot with `next_flow` resumes redirected flow path
4. Integration:
   - flow A jumps to flow B
   - self-jump loop stops at max redirects

## Non-goals
- No changes to `greentic-interfaces` ABI in this PR.
- No pack-to-pack jumps.
- No call/return control stack.

## Acceptance criteria
- Jump control works only via explicit namespaced marker.
- Existing flows (without marker) behave exactly as before.
- Resume uses persisted redirect target when present.
- Redirect loops are bounded via durable state counter.
- No unrelated refactors.
