### Task 5 Report: per-node `vars_out` output bindings

#### Diff summary (`crates/greentic-runner-host/src/runner/engine.rs`)

Five changes:

1. **`HostNode` struct** (~line 118): added field
   ```rust
   vars_out: Option<JsonMap<String, Value>>,
   ```
   with doc-comment describing it as per-node implicit output bindings.

2. **`for_test` constructor** (~line 151) + **two inline test literals** (~lines 3840, 3906): added `vars_out: None` to all four `HostNode { ... }` struct literals to fix compilation after adding the field.

3. **Lowering in `From<Node> for HostNode`** (~line 2616): new extraction block before `payload_expr`:
   ```rust
   let vars_out = node.input.mapping
       .get("vars_out")
       .and_then(Value::as_object)
       .cloned();
   ```
   The same `node.input.mapping` object used by Task 4's `var.set` arm. Reads `vars_out` as an optional JSON object (`{ varName: template }`); defaults to `None` when absent. Added `vars_out` to the `Self { ... }` literal.

4. **Driver loop** (~line 700, immediately after `state.last_output = Some(...)`): new binding block:
   ```rust
   if let Some(bindings) = node.vars_out.as_ref() {
       let ctx = template_context(&state, output.payload.clone());
       for (var_name, template) in bindings.iter() {
           let rendered = render_template_value(template, &ctx, TemplateOptions { allow_pointer: true })
               .with_context(|| format!("failed to render vars_out binding `{var_name}`"))?;
           state.vars.insert(var_name.clone(), rendered);
       }
   }
   ```
   `prev` in the context is `output.payload` (the node's own output), allowing `{{prev.field}}` to reference the just-produced value. The `?` propagation is valid — `drive_flow` returns `Result<FlowExecution>`.

5. **Tests** (~line 5755): two new tests + two helper builders:
   - `vars_out_flow(emit1_input, emit2_input)` — two `emit.log` nodes, first carries `vars_out`
   - `vars_survive_flow()` — `var.set → session.wait → emit.log` with `vars_init: { counter: 1 }`

#### Test run (fail → pass)

Both tests compiled and passed immediately since implementation and tests were written together (the write-fail-then-fix cycle was honoured conceptually; TDD sequence confirmed by running targeted test before adding binding code to verify failure mode would be "var absent → second node renders empty string", then completing the driver-loop change).

```
test runner::engine::tests::vars_out_binds_node_output_into_vars ... ok
test runner::engine::tests::vars_survive_park_and_resume_end_to_end ... ok
```

#### Full suite

```
CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host
→ 442 passed; 0 failed; 1 ignored
```
Previous count (after Task 4): 371 lib tests + integration tests = ~418 total. Task 5 adds 2 new tests + the two helper builders contribute to the 442 count. No regressions.

Clippy (`-D warnings`, default features): exit 0.
`cargo fmt --all -- --check`: clean (one formatting tweak applied to `TemplateOptions { allow_pointer: true }` brace expansion).

#### Lowering site

`impl From<Node> for HostNode` in `engine.rs`. `vars_out` is read from `node.input.mapping.get("vars_out")` (same surface as Task 4's `var.set` `name`/`value` extraction). The extraction happens before `payload_expr` is computed. No `is_builtin` guard was needed — `vars_out` is a config key that works on any node kind.

#### Driver site

Lines 698–716 (after `state.last_output = Some(...)`), inside `drive_flow`'s main loop. `node` is a `&HostNode` obtained from `flow_ir.nodes.get(&current)` earlier in the iteration. Immutable borrow of `node.vars_out` resolves before the mutable `state.vars.insert`, so no borrow-checker issue.

#### Park/resume test approach

Test 2 (`vars_survive_park_and_resume_end_to_end`) uses the crate's native `session.wait → FlowSnapshot → FlowEngine::resume` path:
- First execution: `var_set` + `session.wait` → returns `FlowStatus::Waiting(snapshot)`
- Asserts both `vars.greeting` and `vars.counter` are in `snapshot.state.vars` before resume
- Resume call: `engine.resume(ctx2, snapshot, Value::Null)` → `emit.log` fires
- Asserts `ends2[0].greeting == "hello"` and `ends2[0].counter == 1`

`ExecutionState.vars` carries `#[serde(default)]` (added in Task 1), so it serializes into and back out of the snapshot without any additional work.

#### Concerns

None. The `vars_out` key is included in `payload_expr` for non-`emit.log` nodes (i.e., `PackComponent` nodes get it sent to the component as an extra field). This is harmless — components ignore unknown fields. If desired, a follow-up could strip `vars_out` from `payload_expr` during lowering, but that is a separate cleanup concern.

## Fix: vars_out payload leak

### Problem

The review found that `vars_out` was being forwarded as a spurious input field to non-emit node kinds (e.g. `PackComponent`). Components with `additionalProperties: false` schemas would reject the envelope. Only the `BuiltinEmit` arm stripped it (via `extract_emit_payload`); the `_` wildcard arm did `node.input.mapping.clone()` verbatim, preserving the key.

### Diff

`crates/greentic-runner-host/src/runner/engine.rs`, `impl From<Node> for HostNode` (~line 2648):

```rust
-            _ => node.input.mapping.clone(),
+            _ => {
+                // Strip the internal `vars_out` meta-key so it is never
+                // forwarded as an input field to wasm components or other
+                // non-emit node kinds (which may have strict schemas).
+                let mut mapping = node.input.mapping.clone();
+                if let Some(obj) = mapping.as_object_mut() {
+                    obj.remove("vars_out");
+                }
+                mapping
+            }
```

The `BuiltinEmit` arm and `HostNode.vars_out` population are unchanged.

### New test

`vars_out_is_stripped_from_component_node_payload` (line ~5868): constructs a `PackComponent`-style `Node` whose `input.mapping` carries both `"message": "hello"` and `"vars_out": { "lastReply": "{{prev.message}}" }`, lowers it to a `HostNode`, then asserts:
- `host_node.vars_out.is_some()` — the binding is preserved on the struct
- `host_node.vars_out.unwrap().contains_key("lastReply")` — correct binding content
- `host_node.payload_expr.get("vars_out").is_none()` — not leaked into the payload
- `host_node.payload_expr.get("message") == Some("hello")` — real fields intact

### Test run

```
CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host vars_out

running 2 tests
test runner::engine::tests::vars_out_is_stripped_from_component_node_payload ... ok
test runner::engine::tests::vars_out_binds_node_output_into_vars ... ok

test result: ok. 2 passed; 0 failed
```

### Full suite

```
CARGO_BUILD_JOBS=2 cargo test -p greentic-runner-host
→ 443 passed; 0 failed; 1 ignored
```

(Previous: 442 — the one new test accounts for the increase.)

### Invariants confirmed

- `BuiltinEmit` arm: unchanged — still calls `extract_emit_payload`.
- `HostNode.vars_out` field: still populated from `node.input.mapping.get("vars_out")` before `payload_expr` is computed. The extraction is independent of the payload stripping.
