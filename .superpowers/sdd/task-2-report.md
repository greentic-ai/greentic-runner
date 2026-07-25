# Task 2 Report: thread `sorla:` through the three tool seams + builder (aw-runtime)

## Status: COMPLETE

## Commit
See `git log -1` on branch `feat/sorla-agentic-tool` (commit created by this task; message per brief Step 8:
`feat(aw-runtime): route sorla: tool refs through list/missing/dispatch + with_sorla_source`).

## TDD cycle
**RED** → Added `dispatch_routes_sorla_ref` and `list_includes_sorla_ref` to `tools.rs`'s test module,
referencing the new `sorla` parameter (5th positional arg) on `list_tools_for_llm`/`dispatch_tool_call`
before those signatures existed — this, plus every other existing call site in the crate, would not compile
until the signature changes landed.

**GREEN** → After threading `sorla` through `list_tools_for_llm`, `missing_tools`, `dispatch_tool_call`
(lib.rs field/builder/export, tools.rs arms + all existing test call sites, loop.rs catalog resolution +
threading, tests/tools_live.rs integration test call sites), whole-crate test run is green (see below).

## Files changed
- `crates/greentic-aw-runtime/src/lib.rs` — `sorla` field on `AgentRuntime` (beside `flows`), `sorla: None`
  constructor default, `with_sorla_source` builder (beside `with_flow_source`), `pub use sorla_source::{...}`
  (beside the `flow_source` `pub use`).
- `crates/greentic-aw-runtime/src/tools.rs` — `use crate::sorla_source::SorlaToolCatalog;` import; `sorla`
  param (last catalog param) + `sorla:` arm (immediately after `component:`, before `flow:`) added to
  `list_tools_for_llm`, `missing_tools`, `dispatch_tool_call`; doc comments extended to mention the sorla
  seam; every existing test call site of the three functions updated with the extra `None` in the new
  position; two new tests added (`list_includes_sorla_ref`, `dispatch_routes_sorla_ref`) using
  `crate::sorla_source::test_support::FakeInvoker` + `SorlaToolCatalog::for_tests`.
- `crates/greentic-aw-runtime/src/loop.rs` — `sorla_catalog` resolution (mirrors `flow_catalog`, TTL-cached
  via `runtime.sorla`), threaded into `missing_tools` (`.as_deref()`), `list_tools_for_llm` (`.as_deref()`),
  and `dispatch_tool_call` (`.clone()`).
- `crates/greentic-aw-runtime/tests/tools_live.rs` — updated the two existing call sites
  (`list_tools_for_llm`, `dispatch_tool_call`) with the extra `None` (in-crate integration test, otherwise
  would not compile).

Out of scope per the plan (Task 3): `crates/greentic-runner-host/src/runner/graph_node.rs` still calls the
4-catalog-arg `dispatch_tool_call` — that crate is a separate package and is not built by
`cargo test -p greentic-aw-runtime`; it is Task 3's job (`SorxHttpInvoker` + `sorla_source_from_env` +
runner-host wiring) to update it.

## Verification (real output)

```
$ cargo test -p greentic-aw-runtime -- --nocapture
...
test tools::tests::list_includes_sorla_ref ... ok
...
test tools::tests::dispatch_routes_sorla_ref ... ok  (in the async/tokio group)
...
test result: ok. 344 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.36s
     Running tests/tools_live.rs ... test result: ok. 1 passed; 0 failed
   Doc-tests greentic_aw_runtime ... test result: ok. 1 passed; 0 failed
(all other integration test binaries: ok, 0 failed)
```

```
$ cargo fmt -p greentic-aw-runtime -- --check
(clean, no diff)
```

```
$ cargo clippy -p greentic-aw-runtime --all-targets -- -D warnings
    Checking greentic-aw-runtime v1.3.0-research.0 (...)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.66s
(no warnings)
```

## Concerns
- `greentic-runner-host` (a separate workspace package) still calls `dispatch_tool_call` with the
  pre-sorla 4-catalog-arg signature (`graph_node.rs:1601`) — that package will not build until Task 3 adds
  the sixth positional `sorla` arg there. This is expected/in-scope-for-Task-3 per `progress.md`, not a
  regression introduced here; `cargo test -p greentic-aw-runtime` (the specified gate) is unaffected since
  cargo does not build reverse-dependents for a `-p`-scoped run.
- No other concerns; the `sorla:` dispatch arm mirrors the `component:` arm exactly (never returns `Err`,
  `None` catalog → warn + `{"error": ...}` value).
