# Runner reads `dw-agents.json` sidecar for deployed AgentConfig (Design)

- **Date:** 2026-06-21
- **Status:** Design approved, ready for planning
- **Surface:** greentic-runner (`greentic-runner-host`). Runner-only.
- **Part of:** AW Composer roadmap SP-5. This is the Path-B bridge that makes deployed short-term memory (and, once the designer maps it, knowledge) actually reach runtime.

## 1. Background / problem

Deployed agentic-worker config does not reach the runtime today. Verified flow:

- The designer builds `.gtpack`s by shelling to the external `greentic-pack` binary, which pins **greentic-types 0.5** — its `PackManifest` has **no `agents` field**. The designer injects `agents:` into `pack.yaml` expecting packc to write `manifest.agents`, but packc 0.5 silently drops it.
- The designer ALSO embeds the agent configs as a `dw-agents.json` sidecar in the pack (`embed_dw_agents`).
- The runner (greentic-types ≥1.1) reads agent configs **only** from `manifest.agents` (`PackRuntime::manifest_agent_blobs` → `agent_configs_from_manifest`, used in `runtime.rs`). It never reads the `dw-agents.json` sidecar.

Result: `manifest.agents` is empty for designer-built packs, so deployed workers get no `AgentConfig` — short-term memory and knowledge never activate.

The proper fix (bump greentic-pack to types ≥1.1 + republish the binary) is foundational and release-gated. This spec is the **pragmatic, code-only bridge**: have the runner also read the `dw-agents.json` sidecar the designer already writes.

## 2. Goal

When a pack's `manifest.agents` is empty (or missing an agent), the runner falls back to the pack's `dw-agents.json` sidecar for that agent's `AgentConfig`. This makes designer-built deployed workers receive their config — immediately enabling short-term memory (the designer already maps `memory.short_term`) and, once the designer maps knowledge, knowledge too.

## 3. Non-goals

- No greentic-pack / greentic-types bump, no binary republish.
- No change to `agent_configs_from_manifest` deserialization (serde defaults already tolerate missing `knowledge`/`guardrails`).
- `manifest.agents` remains authoritative — the sidecar only fills gaps (so once greentic-pack is fixed, behavior is unchanged).

## 4. Design

### 4.1 Sidecar reader — `pack.rs`
Add to `PackRuntime` (mirroring `read_agent_graph_sidecar`, reusing the existing `read_pack_file`):

```rust
/// Raw agent-config blobs from the optional `dw-agents.json` sidecar.
/// Designer-built packs (old greentic-pack, no `manifest.agents`) embed their
/// AgentConfig map here. Returns an empty map when absent or unparseable
/// (lenient, mirroring `manifest_agent_blobs`).
pub fn dw_agents_sidecar_blobs(&self) -> BTreeMap<String, serde_json::Value>
```

It calls `self.read_pack_file("dw-agents.json")`; on `Some(bytes)` parse `serde_json::from_slice::<BTreeMap<String, Value>>`; on parse error log `warn!` and return empty; on `None` return empty.

### 4.2 Merge at load — `runtime.rs`
In the `agentic-worker` block (~line 214), where each pack's blobs are collected, fill from the sidecar for any agent_id the manifest didn't carry:

```rust
let mut blobs = pack.manifest_agent_blobs();
for (agent_id, blob) in pack.dw_agents_sidecar_blobs() {
    blobs.entry(agent_id).or_insert(blob);   // manifest wins; sidecar fills gaps
}
if blobs.is_empty() { continue; }
```

The rest (`agent_configs_from_manifest`, cross-pack collision logging, `merge_agent_sources` with operator config) is unchanged.

## 5. Data flow

designer `embed_dw_agents` → `dw-agents.json` in `.gtpack` → runner `dw_agents_sidecar_blobs()` → merged into per-pack blobs (manifest priority) → `agent_configs_from_manifest` → `AgentConfig` (incl. `memory.short_term`) → loop honors it (short-term tools active; knowledge once the designer maps it).

## 6. Error handling

- Absent sidecar → empty map (the common case once greentic-pack is fixed). No error.
- Unparseable sidecar → `warn!` + empty map; pack load never aborts (mirrors the manifest/agent-graph lenient posture).
- Per-blob malformed AgentConfig → already skipped with `warn!` by `agent_configs_from_manifest`.

## 7. Testing

- **Unit (`pack.rs`):** build a temp materialized pack dir containing `dw-agents.json` (a `{agent_id: AgentConfig-json}` map) → `dw_agents_sidecar_blobs()` returns the parsed map; absent file → empty; malformed JSON → empty (+ warn). (Mirror existing pack-file/sidecar tests; the `.gtpack` zip path is already covered by `read_pack_file`.)
- **Merge (`agent_node`/`runtime` unit, if a seam allows):** given manifest blobs `{a}` and sidecar blobs `{a (different), b}`, the merge yields `a` from manifest (priority) and `b` from sidecar. If the merge is inline in `runtime.rs` and not unit-testable in isolation, extract a tiny pure helper `fn merge_sidecar_into(blobs: &mut BTreeMap<..>, sidecar: BTreeMap<..>)` and unit-test that.

## 8. Risks

- Low — additive, gap-fill only, manifest stays authoritative, lenient on errors. Reuses the proven `read_pack_file` path. Once greentic-pack is bumped (Path A, later), `manifest.agents` is populated and the sidecar fill becomes a no-op.
