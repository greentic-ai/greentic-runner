# PR-RUN-JUMP-01: Audit runner-host for cursor & outcome plumbing (no behavior change)
Date: 2026-02-24
Repo: greentic-runner (crates/greentic-runner-host)

## Objective
Do a focused audit to locate the **single, correct insertion points** for a Jump platform primitive with minimal change.

This PR must:
- add **no new behavior**
- only add docs/notes/tests that confirm current behavior
- identify exact types/files to modify in PR-02

## What we already know from grep
- `snapshot.next_node` exists and is used to construct a cursor and drive execution.
- No `next_flow` is evident.
- Engine tests validate `next_node` movement and multi-wait/redis LB behavior.

## Lockstep constraints for PR-02
To avoid drift between audit and implementation docs, PR-02 must follow these constraints:
- Introduce a small typed control channel in runner engine (`NodeControl`), including reserved `Respond`.
- Recognize Jump only via explicit namespaced marker (e.g. `greentic_control.action == "jump"`), never by generic `flow/node` fields.
- Keep default-node fallback semantics identical to runtime today: `entrypoints["default"]` then first node, not `"start"`.
- Store redirect loop guard in durable execution state (serialized with snapshot), not trace/meta fields.
- Extend `FlowSnapshot` with `next_flow` and make snapshot redirect target authoritative during resume.

## Audit tasks (Codex: execute all, no extra questions unless blocked)
1) Identify the persistent cursor/snapshot types
   - Find where `snapshot.next_node` is defined and serialized.
   - Record:
     - type name(s) (e.g., `FlowSnapshot`, `TypesSnapshot`, etc.)
     - where it is stored (in-memory + persisted store)
     - whether it lives in runner-host types or in greentic-types.

2) Identify the node execution outcome type
   - Locate the enum/struct returned by “execute node/component” that the runner matches on.
   - Common patterns to look for:
     - `Outcome`, `NodeOutcome`, `StepOutcome`, `EngineOutcome`, `InvocationResult`
   - If no full control enum exists, identify the minimal struct currently used and the exact insertion point to introduce `NodeControl`.

3) Identify where payload/metadata/state are threaded between nodes
   - Track the path for:
     - inbound envelope to component invocation
     - component return value to next node state
   - Confirm where `metadata.trace` (or equivalent) is read/written today.

4) Identify where pack flow/node graph is available for validation
   - Find how runner loads flow IR/graph for the current pack.
   - Identify an existing helper that can answer:
     - `flow_exists(flow_id)`
     - `node_exists(flow_id, node_id)`
     - `default_start_node(flow_id)` (if not, confirm the convention used)

5) Add “audit assertions” tests (no new behavior)
   - Add 1–2 tiny tests that assert:
     - there is exactly one “advance cursor” code path
     - `next_node` is the only persisted cursor today
   - These tests can be doc-tests or unit tests that just pin structure, not behavior.

## Deliverables
- `docs/jump_primitive_audit.md` summarizing:
  - exact file paths and type names for snapshot/cursor/outcome/validation
  - recommended minimal diffs for PR-02 (bullet list), explicitly including:
    - namespaced jump marker requirement
    - reserved `Respond` in control channel
    - durable redirect guard location
    - `next_flow` snapshot/resume authority
- Optional: a small comment block near the outcome match arm explaining where Jump will be added.

## Acceptance criteria
- CI passes; no behavioral change.
- We have a clear, validated map of where to add Jump with minimal surface area.
