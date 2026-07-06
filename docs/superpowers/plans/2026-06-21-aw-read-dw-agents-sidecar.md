# Runner reads `dw-agents.json` sidecar — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the runner fall back to a pack's `dw-agents.json` sidecar for agent configs when `manifest.agents` is empty, so designer-built deployed workers receive their `AgentConfig` (enabling short-term memory now, knowledge once the designer maps it).

**Architecture:** Add a lenient sidecar-blob reader on `PackRuntime` (reuses existing `read_pack_file`), a tiny pure merge helper (manifest wins, sidecar fills), and wire both into the `agentic-worker` agent-collection block in `runtime.rs`.

**Tech Stack:** Rust (edition 2024), serde_json, zip (via existing `read_pack_file`). Crate: `greentic-runner-host`.

## Global Constraints

- Rust 1.94, edition 2024. No `.unwrap()`/`.expect()` in non-test code.
- `manifest.agents` stays authoritative; the sidecar only fills agent_ids the manifest lacks (so the change is a no-op once greentic-pack is bumped).
- Lenient: absent/unparseable sidecar → empty map + (for parse error) a `warn!`; never abort pack load.
- Sidecar entry name exactly `dw-agents.json`; it is a JSON object `{ "<agent_id>": <AgentConfig-json>, ... }`.
- Conventional commits; NO Claude/AI co-author or attribution.
- Work in worktree `.worktrees/sidecar-agents` on branch `feat/aw-read-dw-agents-sidecar`. Do NOT touch the main worktree.
- Disk may be tight / parallel sessions may hold the cargo lock. Prefer scoped builds: `cargo build -p greentic-runner-host`, `cargo test -p greentic-runner-host <filter>`, `cargo clippy -p greentic-runner-host --lib -- -D warnings`. Avoid `--workspace`. If a hook runs workspace clippy and is blocked/disk-fails, run the scoped checks + `cargo fmt --all` and commit `--no-verify`, noting it.

## Reference facts (verified)

- `crates/greentic-runner-host/src/pack.rs`: `pub fn manifest_agent_blobs(&self) -> BTreeMap<String, serde_json::Value>` returns `manifest.agents` (empty for old-types packs). `pub fn read_pack_file(&self, name: &str) -> Option<Vec<u8>>` reads a pack-relative file (materialized dir first, then `.gtpack` zip), lenient on errors. `pub fn read_agent_graph_sidecar(&self) -> Option<Vec<u8>> { self.read_pack_file("agent-graph.json") }` is the mirror target.
- `crates/greentic-runner-host/src/runner/agent_node.rs`: `pub fn agent_configs_from_manifest(pack_id: &str, blobs: &BTreeMap<String, Value>) -> HashMap<String, AgentConfig>` (lenient per-blob). `merge_agent_sources(pack_agents, operator_agents)` merges operator overrides.
- `crates/greentic-runner-host/src/runtime.rs` (~200-235, behind `#[cfg(feature = "agentic-worker")]`): for each pack, `let blobs = pack.manifest_agent_blobs(); if blobs.is_empty() { continue; }` then `agent_configs_from_manifest(&pack_id, &blobs)` → collected into `pack_agents`, then `merge_agent_sources(pack_agents, config.agents.clone())`.
- The designer embeds the sidecar via `embed_dw_agents` → `dw-agents.json` = a `BTreeMap<String, AgentConfig>` serialized to JSON.

---

## Task 1: `dw_agents_sidecar_blobs()` on `PackRuntime`

**Files:**
- Modify: `crates/greentic-runner-host/src/pack.rs`
- Test: `crates/greentic-runner-host/src/pack.rs` (its `#[cfg(test)]` tests)

**Interfaces:**
- Produces: `pub fn dw_agents_sidecar_blobs(&self) -> std::collections::BTreeMap<String, serde_json::Value>`.

- [ ] **Step 1: Write the failing test**

Find the existing pack-file/sidecar tests in `pack.rs` (search for `read_pack_file` or `read_agent_graph_sidecar` tests, or tests that build a temp materialized pack dir). Mirror that harness. Add:

```rust
    #[test]
    fn dw_agents_sidecar_blobs_reads_map_from_pack_dir() {
        // Build a materialized pack dir with a dw-agents.json map.
        let dir = tempfile::tempdir().unwrap();
        let agents = serde_json::json!({
            "greeter": { "agent_id": "greeter", "system_prompt": "hi", "tools": [],
                         "llm": { "provider": "openai", "model": "gpt-4o-mini" } }
        });
        std::fs::write(dir.path().join("dw-agents.json"),
                       serde_json::to_vec(&agents).unwrap()).unwrap();
        let pack = pack_runtime_for_dir(dir.path()); // use this file's existing dir->PackRuntime test ctor
        let blobs = pack.dw_agents_sidecar_blobs();
        assert!(blobs.contains_key("greeter"));
        assert_eq!(blobs["greeter"]["agent_id"], "greeter");
    }

    #[test]
    fn dw_agents_sidecar_blobs_absent_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let pack = pack_runtime_for_dir(dir.path());
        assert!(pack.dw_agents_sidecar_blobs().is_empty());
    }

    #[test]
    fn dw_agents_sidecar_blobs_malformed_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dw-agents.json"), b"not json").unwrap();
        let pack = pack_runtime_for_dir(dir.path());
        assert!(pack.dw_agents_sidecar_blobs().is_empty());
    }
```

Replace `pack_runtime_for_dir(...)` with whatever constructor the existing `pack.rs` tests use to make a `PackRuntime` from a directory (read the surrounding tests and reuse their helper — do not invent a new ctor). If the existing tests only exercise the `.gtpack` zip path, build the pack the same way they do and place `dw-agents.json` accordingly.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p greentic-runner-host dw_agents_sidecar 2>&1 | tail -20`
Expected: FAIL — `dw_agents_sidecar_blobs` not found.

- [ ] **Step 3: Implement the reader**

In `pack.rs`, next to `read_agent_graph_sidecar`, add:

```rust
    /// Raw agent-config blobs from the optional `dw-agents.json` sidecar.
    ///
    /// Designer-built packs (old greentic-pack, which cannot populate
    /// `manifest.agents`) embed their `AgentConfig` map here. Returns an empty
    /// map when the sidecar is absent or unparseable (lenient, mirroring
    /// [`PackRuntime::manifest_agent_blobs`]) so a damaged sidecar never aborts
    /// pack loading. `manifest.agents` remains authoritative; callers fill only
    /// the agent_ids the manifest did not carry.
    pub fn dw_agents_sidecar_blobs(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        let Some(bytes) = self.read_pack_file("dw-agents.json") else {
            return std::collections::BTreeMap::new();
        };
        match serde_json::from_slice::<std::collections::BTreeMap<String, serde_json::Value>>(&bytes)
        {
            Ok(map) => map,
            Err(error) => {
                tracing::warn!(error = %error, "ignoring malformed dw-agents.json sidecar");
                std::collections::BTreeMap::new()
            }
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p greentic-runner-host dw_agents_sidecar 2>&1 | tail -20`
Expected: PASS — all three.

- [ ] **Step 5: Commit**

```bash
git add crates/greentic-runner-host/src/pack.rs
git commit -m "feat(runner-host): read agent configs from dw-agents.json sidecar"
```

---

## Task 2: Merge helper + wire into pack load

**Files:**
- Modify: `crates/greentic-runner-host/src/runner/agent_node.rs` (add pure `merge_sidecar_into` + test)
- Modify: `crates/greentic-runner-host/src/runtime.rs` (call sidecar + merge)

**Interfaces:**
- Consumes: `pack.dw_agents_sidecar_blobs()` (Task 1).
- Produces: `pub fn merge_sidecar_into(blobs: &mut BTreeMap<String, serde_json::Value>, sidecar: BTreeMap<String, serde_json::Value>)` — inserts each sidecar entry only when the key is absent (manifest wins).

- [ ] **Step 1: Write the failing test**

In `crates/greentic-runner-host/src/runner/agent_node.rs` tests (`#[cfg(test)] mod tests`):

```rust
        #[test]
        fn merge_sidecar_fills_only_missing_keys() {
            use std::collections::BTreeMap;
            let mut blobs: BTreeMap<String, serde_json::Value> =
                BTreeMap::from([("a".to_string(), serde_json::json!({"from": "manifest"}))]);
            let sidecar: BTreeMap<String, serde_json::Value> = BTreeMap::from([
                ("a".to_string(), serde_json::json!({"from": "sidecar"})), // must NOT override
                ("b".to_string(), serde_json::json!({"from": "sidecar"})), // must be added
            ]);
            super::merge_sidecar_into(&mut blobs, sidecar);
            assert_eq!(blobs["a"]["from"], "manifest"); // manifest wins
            assert_eq!(blobs["b"]["from"], "sidecar"); // gap filled
            assert_eq!(blobs.len(), 2);
        }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p greentic-runner-host merge_sidecar 2>&1 | tail -15`
Expected: FAIL — `merge_sidecar_into` not found.

- [ ] **Step 3: Implement the helper**

In `agent_node.rs`, next to `agent_configs_from_manifest`:

```rust
/// Fill `blobs` from a `dw-agents.json` `sidecar` map, inserting each entry only
/// when its agent_id is absent — so `manifest.agents` stays authoritative and the
/// sidecar only bridges packs whose manifest could not carry agents.
pub fn merge_sidecar_into(
    blobs: &mut std::collections::BTreeMap<String, serde_json::Value>,
    sidecar: std::collections::BTreeMap<String, serde_json::Value>,
) {
    for (agent_id, blob) in sidecar {
        blobs.entry(agent_id).or_insert(blob);
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p greentic-runner-host merge_sidecar 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Wire into `runtime.rs`**

In `crates/greentic-runner-host/src/runtime.rs`, in the `#[cfg(feature = "agentic-worker")]` block, update the import line to include the helper and change the per-pack blob collection:

Change the use:
```rust
            use crate::runner::agent_node::{
                agent_configs_from_manifest, merge_agent_sources, merge_sidecar_into,
            };
```

Replace:
```rust
                let blobs = pack.manifest_agent_blobs();
                if blobs.is_empty() {
                    continue;
                }
```
with:
```rust
                let mut blobs = pack.manifest_agent_blobs();
                // Bridge: designer-built packs cannot populate `manifest.agents`
                // (old greentic-pack); they embed a `dw-agents.json` sidecar.
                // Fill any agent_id the manifest lacked (manifest stays authoritative).
                merge_sidecar_into(&mut blobs, pack.dw_agents_sidecar_blobs());
                if blobs.is_empty() {
                    continue;
                }
```

- [ ] **Step 6: Build + scoped clippy**

Run: `cargo build -p greentic-runner-host 2>&1 | tail -8`
Expected: compiles.

Run: `cargo clippy -p greentic-runner-host --lib -- -D warnings 2>&1 | tail -6`
Expected: clean.

Run: `cargo test -p greentic-runner-host merge_sidecar dw_agents_sidecar 2>&1 | tail -15`
Expected: PASS (Task 1 + Task 2 tests).

- [ ] **Step 7: Commit**

```bash
git add crates/greentic-runner-host/src/runner/agent_node.rs crates/greentic-runner-host/src/runtime.rs
git commit -m "feat(runner-host): fill agent configs from dw-agents.json sidecar at pack load"
```

---

## Manual verification (after Task 2)

A designer-built pack (with an empty `manifest.agents` but a `dw-agents.json` sidecar carrying `memory.short_term`) loads its agent at runtime: the worker advertises `remember`/`recall` and short-term memory works. A pack with populated `manifest.agents` is unaffected (sidecar fill is a no-op for present ids).

## Self-Review (completed during planning)

- **Spec coverage:** §4.1 reader → Task 1; §4.2 merge + wiring → Task 2; §7 testing folded in.
- **Placeholder scan:** Task 1's test ctor references the file's existing dir→PackRuntime helper (named explicitly to reuse, not invent) — the only thing the implementer must look up; everything else is complete code.
- **Type consistency:** `dw_agents_sidecar_blobs` and `merge_sidecar_into` use `BTreeMap<String, serde_json::Value>`, matching `manifest_agent_blobs`/`agent_configs_from_manifest`'s blob type throughout.
