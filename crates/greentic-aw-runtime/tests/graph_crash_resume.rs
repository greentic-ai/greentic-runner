//! Simulates a crash mid-run: the first executor drives until the tool node
//! has recorded its visit, then "crashes" (its next agent turn errors and the
//! instance is dropped). A second, fresh executor resumes the same run_id
//! against the same store and must finish WITHOUT re-invoking the
//! agent/tool effects that already completed and were recorded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use greentic_aw_runtime::graph::{
    AgentTurnFn, AgentTurnRequest, AgentTurnResult, CheckpointStore, GraphConfig, GraphExecError,
    GraphExecutor, InMemoryCheckpointStore, RunStatus, SupervisorFn, SupervisorRequest,
    SupervisorResult, ToolCallRequest, ToolFn,
};
use greentic_aw_runtime::tenant::TenantContext;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn tenant() -> TenantContext {
    TenantContext::new("test", "dev")
}

/// Returns the triage graph JSON.
///
/// Inlined here because integration tests cannot access `crate::graph::test_fixtures`
/// (a `pub(crate)` module).  The graph is identical to the unit-test fixture:
/// agent → lookup(tool) → router(maxIterations=3) → respond.
fn triage_json() -> String {
    serde_json::json!({
        "schemaVersion": 1,
        "entry": "agent",
        "nodes": [
            {"id": "agent",  "kind": "agent",  "systemPrompt": "You triage.", "model": "gpt-4o-mini", "tools": []},
            {"id": "lookup", "kind": "tool",   "toolName": "kb.search"},
            {"id": "router", "kind": "router", "maxIterations": 3},
            {"id": "respond","kind": "respond"}
        ],
        "edges": [
            {"from": "agent",  "to": "lookup"},
            {"from": "lookup", "to": "router"},
            {"from": "router", "to": "agent",  "branch": "loop"},
            {"from": "router", "to": "respond","branch": "resolved"}
        ]
    })
    .to_string()
}

fn triage_cfg() -> GraphConfig {
    GraphConfig::from_json(&triage_json()).expect("triage fixture is valid")
}

/// Agent closure that succeeds (unresolved) on call 1 and errors on call 2+.
fn agent_fn_crash_on_second(counter: Arc<AtomicU32>) -> AgentTurnFn {
    Arc::new(move |req: AgentTurnRequest| {
        let n = counter.fetch_add(1, Ordering::SeqCst) + 1; // 1-indexed
        let node = req.node_id.clone();
        Box::pin(async move {
            if n == 1 {
                Ok(AgentTurnResult {
                    reply: format!("first pass from {node}"),
                    resolved: false,
                })
            } else {
                Err(GraphExecError::AgentTurn("simulated crash".into()))
            }
        })
    })
}

/// Agent closure that always returns resolved=true.
fn agent_fn_resolves_always(counter: Arc<AtomicU32>) -> AgentTurnFn {
    Arc::new(move |_req: AgentTurnRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(AgentTurnResult {
                reply: "resolved now [[final answer]]".to_owned(),
                resolved: true,
            })
        })
    })
}

/// Tool closure that always succeeds, with a counter.
fn tool_fn_always_ok(counter: Arc<AtomicU32>, hits: u32) -> ToolFn {
    Arc::new(move |_req: ToolCallRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        let v = serde_json::json!({"hits": hits});
        Box::pin(async move { Ok(v) })
    })
}

/// A no-op supervisor fn for tests that do not exercise supervisor nodes.
fn supervisor_fn_unreachable() -> SupervisorFn {
    Arc::new(|_req: SupervisorRequest| {
        Box::pin(async move {
            Err(GraphExecError::Supervisor(
                "supervisor fn should not be called in crash-resume tests".into(),
            ))
        })
    })
}

// ---------------------------------------------------------------------------
// Test 1: resume_after_crash_reexecutes_only_unfinished_work
//
// Phase 1:
//   Executor A runs until agent attempt 2 errors (simulated crash).
//   Store still holds Running status with cursor=agent, visits={agent:1,lookup:1,router:1}.
//   Node-visit ledger: agent/1 + lookup/1 recorded; agent/2 NOT recorded (error
//   fires inside the closure, before record_node_visit is called).
//
// Phase 2:
//   Executor B (fresh closures, same store Arc) resumes the same run.
//   B's agent is invoked for attempt 2 only (attempt 1 was already checkpointed
//   via the cursor, so the drive loop starts at attempt 2 from the stored visits).
//   B's tool is invoked for lookup attempt 2 only (attempt 1 was replayed from
//   the ledger only if load_node_visit returns it; it will, and attempt 2 is fresh).
//
// Guarantee asserted:
//   - agent B count == 1 (only re-runs the failed attempt 2)
//   - tool  B count == 1 (lookup attempt 2 is fresh)
//   - outcome.status == Succeeded
//   - No trail entry from the phase-2 drive has replayed=true (the cursor already
//     advanced past the recorded attempts; replay of agent/1 and lookup/1 would
//     only fire if cursor rewound to those nodes, which it did not).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_after_crash_reexecutes_only_unfinished_work() {
    let store = Arc::new(InMemoryCheckpointStore::default());
    let t = tenant();
    let cfg = triage_cfg();

    // --- Phase 1: executor A ---
    let agent_a_count = Arc::new(AtomicU32::new(0));
    let tool_a_count = Arc::new(AtomicU32::new(0));

    {
        let exec_a = GraphExecutor::new(
            store.clone(),
            agent_fn_crash_on_second(agent_a_count.clone()),
            tool_fn_always_ok(tool_a_count.clone(), 1),
            supervisor_fn_unreachable(),
        );

        let err = exec_a
            .start(&t, "crash-run", &cfg, "hello")
            .await
            .expect_err("start should fail when agent errors on attempt 2");

        assert!(
            matches!(err, GraphExecError::AgentTurn(_)),
            "expected AgentTurn error, got {err:?}"
        );

        // Record must still be Running.
        let rec = store
            .load(&t, "crash-run")
            .await
            .expect("store accessible")
            .expect("record must exist");
        assert_eq!(
            rec.status,
            RunStatus::Running,
            "run must remain Running after effect error (not Failed)"
        );
    }
    // Executor A is now dropped ("process crash").

    // Phase 1 assertion: A's agent was called 2× (call 1 OK, call 2 error).
    assert_eq!(
        agent_a_count.load(Ordering::SeqCst),
        2,
        "executor A agent must have been invoked exactly 2 times"
    );
    // Phase 1 assertion: A's tool was called 1× (lookup attempt 1 completed OK).
    assert_eq!(
        tool_a_count.load(Ordering::SeqCst),
        1,
        "executor A tool must have been invoked exactly 1 time"
    );

    // --- Phase 2: executor B (fresh closures, same store) ---
    let agent_b_count = Arc::new(AtomicU32::new(0));
    let tool_b_count = Arc::new(AtomicU32::new(0));

    let exec_b = GraphExecutor::new(
        store.clone(),
        agent_fn_resolves_always(agent_b_count.clone()),
        tool_fn_always_ok(tool_b_count.clone(), 2),
        supervisor_fn_unreachable(),
    );

    let outcome = exec_b
        .resume(&t, "crash-run")
        .await
        .expect("resume must succeed");

    // Outcome status.
    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "resumed run must complete with Succeeded"
    );

    // Cross-instance exactly-once guarantee: only the previously-FAILED attempt
    // (agent attempt 2) is re-executed; attempt 1 is skipped because the cursor
    // already advanced past it in the stored checkpoint.
    assert_eq!(
        agent_b_count.load(Ordering::SeqCst),
        1,
        "executor B agent must be invoked exactly 1 time (only attempt 2 re-runs)"
    );
    // Lookup attempt 2 is a fresh invocation (attempt 1 replayed from ledger,
    // but the cursor was past lookup in the stored state so attempt 1 is not
    // re-driven; attempt 2 is the new call on B).
    assert_eq!(
        tool_b_count.load(Ordering::SeqCst),
        1,
        "executor B tool must be invoked exactly 1 time (lookup attempt 2 is fresh)"
    );

    // Trail from the phase-2 drive must have no replayed=true entries.
    // (The drive starts from cursor=agent with attempt=2; the ledger replay
    // path is exercised only when load_node_visit returns Some for the current
    // attempt — but agent/2 was never recorded, and lookup/2 was never
    // recorded, so both are fresh.)
    for entry in &outcome.trail {
        let replayed = entry
            .get("replayed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(
            !replayed,
            "no phase-2 trail entry should be replayed (cursor advanced past all recorded attempts): entry={entry:?}"
        );
    }

    // Trail must contain exactly 4 entries: agent, lookup, router, respond.
    assert_eq!(
        outcome.trail.len(),
        4,
        "phase-2 trail must have 4 entries (agent/2, lookup/2, router/2, respond/1): {:#?}",
        outcome.trail
    );

    // Spot-check attempt numbers in the trail.
    let agent_entry = outcome.trail.iter().find(|e| e["kind"] == "agent").unwrap();
    assert_eq!(
        agent_entry["attempt"], 2,
        "agent entry in phase-2 trail must be attempt 2"
    );
    let lookup_entry = outcome.trail.iter().find(|e| e["kind"] == "tool").unwrap();
    assert_eq!(
        lookup_entry["attempt"], 2,
        "lookup entry in phase-2 trail must be attempt 2"
    );
}

// ---------------------------------------------------------------------------
// Test 2: resume_replays_ledgered_attempt_across_instances
//
// Same crash scenario, but BEFORE phase-2's resume we manually inject a
// node-visit record for ("crash-run2", "agent", 2) — simulating the window
// where the effect ran and was recorded but the checkpoint commit never
// happened (the exact crash-after-record-before-checkpoint window).
//
// Guarantee asserted:
//   - When executor B starts, visit_effect sees agent/2 in the ledger → replayed=true.
//   - agent B count == 0 (closure never invoked for attempt 2, replayed from ledger).
//   - tool  B count == 1 (lookup attempt 2 is still fresh — not in ledger).
//   - outcome.status == Succeeded.
//   - The phase-2 trail entry for agent has replayed=true and attempt==2.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_replays_ledgered_attempt_across_instances() {
    let store = Arc::new(InMemoryCheckpointStore::default());
    let t = tenant();
    let cfg = triage_cfg();

    // --- Phase 1: executor A ---
    let agent_a_count = Arc::new(AtomicU32::new(0));
    let tool_a_count = Arc::new(AtomicU32::new(0));

    {
        let exec_a = GraphExecutor::new(
            store.clone(),
            agent_fn_crash_on_second(agent_a_count.clone()),
            tool_fn_always_ok(tool_a_count.clone(), 1),
            supervisor_fn_unreachable(),
        );

        let err = exec_a
            .start(&t, "crash-run2", &cfg, "hello")
            .await
            .expect_err("start should fail when agent errors on attempt 2");

        assert!(
            matches!(err, GraphExecError::AgentTurn(_)),
            "expected AgentTurn error, got {err:?}"
        );

        // Verify Running status.
        let rec = store
            .load(&t, "crash-run2")
            .await
            .expect("store accessible")
            .expect("record must exist");
        assert_eq!(rec.status, RunStatus::Running, "run must be Running");
    }

    // Phase 1 counters.
    assert_eq!(agent_a_count.load(Ordering::SeqCst), 2, "A agent: 2 calls");
    assert_eq!(tool_a_count.load(Ordering::SeqCst), 1, "A tool: 1 call");

    // --- Synthetic ledger injection ---
    //
    // Simulate the record-before-checkpoint crash window: the agent effect for
    // attempt 2 ran and was durably written, but the process died before the
    // checkpoint (cursor advance + visits update) could be saved.
    //
    // Injecting this entry causes executor B's visit_effect to return it as
    // replayed=true for agent/attempt-2, so the B agent closure is NEVER called.
    let synthetic_result = serde_json::json!({"reply": "from ledger", "resolved": true});
    store
        .record_node_visit(&t, "crash-run2", "agent", 2, &synthetic_result)
        .await
        .expect("synthetic ledger injection must succeed");

    // --- Phase 2: executor B (fresh closures, same store) ---
    let agent_b_count = Arc::new(AtomicU32::new(0));
    let tool_b_count = Arc::new(AtomicU32::new(0));

    let exec_b = GraphExecutor::new(
        store.clone(),
        agent_fn_resolves_always(agent_b_count.clone()),
        tool_fn_always_ok(tool_b_count.clone(), 2),
        supervisor_fn_unreachable(),
    );

    let outcome = exec_b
        .resume(&t, "crash-run2")
        .await
        .expect("resume must succeed");

    // Outcome.
    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "resumed run must complete with Succeeded"
    );

    // Cross-instance replay guarantee: agent attempt 2 was already ledgered →
    // executor B's closure is never invoked for it.
    assert_eq!(
        agent_b_count.load(Ordering::SeqCst),
        0,
        "executor B agent must NOT be invoked at all (attempt 2 replayed from ledger)"
    );
    // Lookup attempt 2 is still fresh (only attempt 1 is in the ledger from phase 1).
    assert_eq!(
        tool_b_count.load(Ordering::SeqCst),
        1,
        "executor B tool must be invoked exactly 1 time (lookup attempt 2 is fresh)"
    );

    // Trail from the phase-2 drive: agent(2, replayed=true), lookup(2, false),
    // router(2, false), respond(1, false) → 4 entries.
    assert_eq!(
        outcome.trail.len(),
        4,
        "phase-2 trail must have 4 entries: {:#?}",
        outcome.trail
    );

    // The agent entry must be marked replayed=true (loaded from ledger).
    let agent_entry = outcome
        .trail
        .iter()
        .find(|e| e["kind"] == "agent")
        .expect("trail must contain an agent entry");
    assert_eq!(
        agent_entry["attempt"], 2,
        "agent trail entry must be attempt 2"
    );
    assert_eq!(
        agent_entry["replayed"], true,
        "agent trail entry must be replayed=true (loaded from the injected ledger entry)"
    );

    // All other trail entries must NOT be replayed.
    for entry in outcome.trail.iter().filter(|e| e["kind"] != "agent") {
        let replayed = entry
            .get("replayed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(
            !replayed,
            "only the agent entry should be replayed; got replayed=true in: {entry:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Inline fixtures (cannot use crate::graph::test_fixtures — pub(crate) only)
// ---------------------------------------------------------------------------

/// schemaVersion 2 parallel graph:
///   entry (agent) → fan (parallel) →[a]→ agent_a →┐
///                                   →[b]→ tool_b  →┤
///                                                   ▼
///                                             meet (join) → respond
fn parallel_v2_json() -> String {
    serde_json::json!({
        "schemaVersion": 2,
        "entry": "entry",
        "nodes": [
            {
                "id": "entry",
                "kind": "agent",
                "systemPrompt": "Kick off the parallel workflow.",
                "model": "gpt-4o-mini",
                "tools": []
            },
            {"id": "fan",     "kind": "parallel"},
            {
                "id": "agent_a",
                "kind": "agent",
                "systemPrompt": "Branch A specialist.",
                "model": "gpt-4o-mini",
                "tools": []
            },
            {
                "id": "tool_b",
                "kind": "tool",
                "toolName": "branch_b.process"
            },
            {"id": "meet",    "kind": "join"},
            {"id": "respond", "kind": "respond"}
        ],
        "edges": [
            {"from": "entry",   "to": "fan"},
            {"from": "fan",     "to": "agent_a", "branch": "a"},
            {"from": "fan",     "to": "tool_b",  "branch": "b"},
            {"from": "agent_a", "to": "meet"},
            {"from": "tool_b",  "to": "meet"},
            {"from": "meet",    "to": "respond"}
        ]
    })
    .to_string()
}

/// schemaVersion 2 supervisor graph (linear, no router loop):
///   sup (supervisor: routes=[billing, tech])
///     →[billing]→ agent_billing → respond
///     →[tech]   → agent_tech    → respond
///
/// Both branches share the same `respond` terminal.
/// No router loop so the run always terminates in one pass.
fn supervisor_linear_json() -> String {
    serde_json::json!({
        "schemaVersion": 2,
        "entry": "sup",
        "nodes": [
            {
                "id": "sup",
                "kind": "supervisor",
                "systemPrompt": "Route to the correct specialist.",
                "model": "gpt-4o-mini",
                "routes": [
                    {"branch": "billing", "description": "Billing and payment questions"},
                    {"branch": "tech",    "description": "Technical support issues"}
                ]
            },
            {
                "id": "agent_billing",
                "kind": "agent",
                "systemPrompt": "You handle billing questions.",
                "model": "gpt-4o-mini",
                "tools": []
            },
            {
                "id": "agent_tech",
                "kind": "agent",
                "systemPrompt": "You handle technical issues.",
                "model": "gpt-4o-mini",
                "tools": []
            },
            {"id": "respond", "kind": "respond"}
        ],
        "edges": [
            {"from": "sup",           "to": "agent_billing", "branch": "billing"},
            {"from": "sup",           "to": "agent_tech",    "branch": "tech"},
            {"from": "agent_billing", "to": "respond"},
            {"from": "agent_tech",    "to": "respond"}
        ]
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Test 3: parallel_resume_after_crash_completes_without_duplicate_effects
//
// Phase 1 (Executor A):
//   - entry agent succeeds.
//   - fan fans out to branch a (agent_a) and branch b (tool_b).
//   - agent_a SUCCEEDS → its result is recorded in the ledger; branch a parks.
//   - tool_b FAILS on first attempt → Err, branch b does NOT park.
//   - parallel region errors out; run stays Running; frontier_json is Some.
//
// Phase 2 (Executor B, same store):
//   - tool_b now SUCCEEDS.
//   - resume() → re-drives only the non-parked branch b from its initial cursor.
//   - Branch a is already parked → skipped entirely; B's agent_a closure is
//     never called (counter == 0, result replayed from the frontier state).
//   - Branch b completes → merge → respond → Succeeded.
//   - Trunk merges both branches' messages (a-reply then b-tool).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_resume_after_crash_completes_without_duplicate_effects() {
    let store = Arc::new(InMemoryCheckpointStore::default());
    let t = tenant();
    let cfg = GraphConfig::from_json(&parallel_v2_json()).expect("parallel v2 fixture is valid");

    // ── Phase 1: Executor A ──────────────────────────────────────────────────

    let a_entry_count = Arc::new(AtomicU32::new(0)); // entry agent
    let a_agent_a_count = Arc::new(AtomicU32::new(0)); // branch-a agent
    let a_tool_b_count = Arc::new(AtomicU32::new(0)); // branch-b tool

    // The single agent_turn fn dispatches on node_id.
    // For "entry": succeed (resolved=false so we proceed to fan).
    // For "agent_a": succeed (resolved=false — branch nodes don't need resolved=true).
    let a_agent_turn: AgentTurnFn = {
        let entry_c = a_entry_count.clone();
        let a_c = a_agent_a_count.clone();
        Arc::new(move |req: AgentTurnRequest| {
            let node = req.node_id.clone();
            let entry_c = entry_c.clone();
            let a_c = a_c.clone();
            Box::pin(async move {
                match node.as_str() {
                    "entry" => {
                        entry_c.fetch_add(1, Ordering::SeqCst);
                        Ok(AgentTurnResult {
                            reply: "entry done".to_owned(),
                            resolved: false,
                        })
                    }
                    "agent_a" => {
                        a_c.fetch_add(1, Ordering::SeqCst);
                        Ok(AgentTurnResult {
                            reply: "reply from agent_a".to_owned(),
                            resolved: false,
                        })
                    }
                    other => Err(GraphExecError::AgentTurn(format!(
                        "unexpected agent node in phase 1: {other}"
                    ))),
                }
            })
        })
    };

    // tool_b in Phase 1: always fails.
    let a_tool: ToolFn = {
        let c = a_tool_b_count.clone();
        Arc::new(move |req: ToolCallRequest| {
            c.fetch_add(1, Ordering::SeqCst);
            let node = req.node_id.clone();
            Box::pin(async move {
                Err(GraphExecError::Tool(format!(
                    "tool_b simulated failure at {node}"
                )))
            })
        })
    };

    {
        let exec_a = GraphExecutor::new(
            store.clone(),
            a_agent_turn,
            a_tool,
            supervisor_fn_unreachable(),
        );

        let err = exec_a
            .start(&t, "par-crash-run", &cfg, "hello parallel")
            .await
            .expect_err("start must fail when tool_b errors");

        assert!(
            matches!(err, GraphExecError::Tool(_)),
            "expected Tool error from branch b, got {err:?}"
        );

        // Record must still be Running.
        let rec = store
            .load(&t, "par-crash-run")
            .await
            .expect("store accessible")
            .expect("record must exist");
        assert_eq!(
            rec.status,
            RunStatus::Running,
            "run must remain Running after branch-b tool failure"
        );

        // frontier_json must be Some (parallel region was entered).
        assert!(
            rec.frontier_json.is_some(),
            "frontier_json must be Some after parallel region crash"
        );

        // Verify counters: entry called once, agent_a called once, tool_b called once.
        assert_eq!(
            a_entry_count.load(Ordering::SeqCst),
            1,
            "phase-1 entry agent must be invoked once"
        );
        assert_eq!(
            a_agent_a_count.load(Ordering::SeqCst),
            1,
            "phase-1 agent_a must be invoked once (branch a succeeded)"
        );
        assert_eq!(
            a_tool_b_count.load(Ordering::SeqCst),
            1,
            "phase-1 tool_b must be invoked once (the failing attempt)"
        );
    }
    // Executor A is now dropped ("process crash").

    // ── Phase 2: Executor B (fresh closures, same store) ─────────────────────

    let b_agent_a_count = Arc::new(AtomicU32::new(0));
    let b_tool_b_count = Arc::new(AtomicU32::new(0));

    // B's agent_turn: agent_a MUST NOT be called (branch a is parked; we count
    // and assert == 0 at the end).  "entry" should also not be called (the entry
    // cursor advanced past fan after phase 1).  If either is called unexpectedly,
    // the counter will reveal it.
    let b_agent_turn: AgentTurnFn = {
        let a_c = b_agent_a_count.clone();
        Arc::new(move |req: AgentTurnRequest| {
            let node = req.node_id.clone();
            a_c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                // If we get here something went wrong; the counter captures it.
                Ok(AgentTurnResult {
                    reply: format!("B agent reply from {node}"),
                    resolved: false,
                })
            })
        })
    };

    // B's tool: tool_b succeeds.
    let b_tool: ToolFn = {
        let c = b_tool_b_count.clone();
        Arc::new(move |_req: ToolCallRequest| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(serde_json::json!({"branch_b_result": "ok"})) })
        })
    };

    let exec_b = GraphExecutor::new(
        store.clone(),
        b_agent_turn,
        b_tool,
        supervisor_fn_unreachable(),
    );

    let outcome = exec_b
        .resume(&t, "par-crash-run")
        .await
        .expect("resume must succeed");

    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "resumed parallel run must complete with Succeeded"
    );

    // THE GUARANTEE: branch a's agent_a must NOT be re-invoked by Executor B.
    // It was parked in the frontier; its result is carried in branch a's state_json.
    assert_eq!(
        b_agent_a_count.load(Ordering::SeqCst),
        0,
        "executor B must NOT invoke agent_a (branch a was already parked in the frontier)"
    );

    // Branch b's tool_b must be invoked exactly once by Executor B (the successful retry).
    assert_eq!(
        b_tool_b_count.load(Ordering::SeqCst),
        1,
        "executor B must invoke tool_b exactly once (the successful retry)"
    );

    // The final reply must come from the respond node (driven after the merge).
    // The merged state must contain both branch messages (agent_a reply + tool_b result).
    // We verify by inspecting the stored record's state.
    let final_rec = store
        .load(&t, "par-crash-run")
        .await
        .expect("store accessible")
        .expect("record must exist");
    assert_eq!(
        final_rec.status,
        RunStatus::Succeeded,
        "stored status must be Succeeded"
    );
    // frontier_json must be None after a successful merge.
    assert!(
        final_rec.frontier_json.is_none(),
        "frontier_json must be None after a successful parallel merge"
    );

    // The merged messages in state_json must include agent_a's reply (branch a)
    // and tool_b's JSON output (branch b), in branch-label order (a before b).
    let state_val: serde_json::Value =
        serde_json::from_str(&final_rec.state_json).expect("state_json is valid JSON");
    let messages = state_val["messages"].as_array().expect("messages is array");
    let contents: Vec<&str> = messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();

    let agent_a_pos = contents
        .iter()
        .position(|c| c.contains("reply from agent_a"))
        .expect("agent_a reply must be in the merged messages");
    let tool_b_pos = contents
        .iter()
        .position(|c| c.contains("branch_b_result"))
        .expect("tool_b result must be in the merged messages");

    assert!(
        agent_a_pos < tool_b_pos,
        "branch a's message must appear before branch b's (branch label 'a' < 'b'): \
         agent_a_pos={agent_a_pos}, tool_b_pos={tool_b_pos}"
    );
}

// ---------------------------------------------------------------------------
// Test 4: supervisor_decision_survives_crash
//
// Phase 1 (Executor A):
//   - sup supervisor routes to "billing" and records the decision in the ledger.
//   - agent_billing FAILS (Err) → run stays Running.
//   - Assert: supervisor closure invoked once; decision is in the ledger.
//
// Phase 2 (Executor B, same store):
//   - B's supervisor closure would route to "tech" if called — but it must NOT
//     be called (decision replayed from the ledger).
//   - agent_billing succeeds.
//   - resume() → Succeeded via the BILLING branch (recorded decision wins).
//   - Assert: B's supervisor counter == 0; final path went through agent_billing.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn supervisor_decision_survives_crash() {
    let store = Arc::new(InMemoryCheckpointStore::default());
    let t = tenant();
    let cfg =
        GraphConfig::from_json(&supervisor_linear_json()).expect("supervisor linear fixture valid");

    // ── Phase 1: Executor A ──────────────────────────────────────────────────

    let a_sup_count = Arc::new(AtomicU32::new(0));
    let a_billing_count = Arc::new(AtomicU32::new(0));

    // Supervisor: always routes to "billing".
    let a_sup: SupervisorFn = {
        let c = a_sup_count.clone();
        Arc::new(move |_req: SupervisorRequest| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(SupervisorResult {
                    branch: "billing".to_owned(),
                    raw_reply: "[[ROUTE:billing]] routing to billing department".to_owned(),
                })
            })
        })
    };

    // agent_billing in Phase 1: always fails (simulated crash).
    let a_agent_turn: AgentTurnFn = {
        let billing_c = a_billing_count.clone();
        Arc::new(move |req: AgentTurnRequest| {
            let node = req.node_id.clone();
            billing_c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Err(GraphExecError::AgentTurn(format!(
                    "simulated crash in {node}"
                )))
            })
        })
    };

    {
        let exec_a = GraphExecutor::new(
            store.clone(),
            a_agent_turn,
            // no tool nodes in this graph
            Arc::new(|_req: ToolCallRequest| {
                Box::pin(async move {
                    Err(GraphExecError::Tool(
                        "tool should not be called in supervisor test".into(),
                    ))
                })
            }),
            a_sup,
        );

        let err = exec_a
            .start(&t, "sup-crash-run", &cfg, "I have a billing question")
            .await
            .expect_err("start must fail when agent_billing errors");

        assert!(
            matches!(err, GraphExecError::AgentTurn(_)),
            "expected AgentTurn error, got {err:?}"
        );

        // Record must still be Running.
        let rec = store
            .load(&t, "sup-crash-run")
            .await
            .expect("store accessible")
            .expect("record must exist");
        assert_eq!(
            rec.status,
            RunStatus::Running,
            "run must remain Running after agent_billing failure"
        );

        // Supervisor must have been called exactly once.
        assert_eq!(
            a_sup_count.load(Ordering::SeqCst),
            1,
            "phase-1 supervisor must be invoked exactly once"
        );
        // agent_billing must have been called exactly once (failed attempt).
        assert_eq!(
            a_billing_count.load(Ordering::SeqCst),
            1,
            "phase-1 agent_billing must be invoked exactly once (the failing attempt)"
        );

        // Verify the supervisor decision was recorded in the node-visit ledger.
        let ledger_entry = store
            .load_node_visit(&t, "sup-crash-run", "sup", 1)
            .await
            .expect("store accessible")
            .expect("supervisor/attempt-1 must be in the ledger");
        let decision: SupervisorResult =
            serde_json::from_value(ledger_entry).expect("ledger entry must deserialize");
        assert_eq!(
            decision.branch, "billing",
            "recorded decision must be 'billing'"
        );
    }
    // Executor A dropped ("process crash").

    // ── Phase 2: Executor B (fresh closures, same store) ─────────────────────

    let b_sup_count = Arc::new(AtomicU32::new(0));
    let b_billing_count = Arc::new(AtomicU32::new(0));

    // B's supervisor: would route to "tech" if called.
    // It MUST NOT be called — the phase-1 decision must be replayed.
    let b_sup: SupervisorFn = {
        let c = b_sup_count.clone();
        Arc::new(move |_req: SupervisorRequest| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(SupervisorResult {
                    branch: "tech".to_owned(),
                    raw_reply: "[[ROUTE:tech]] routing to tech support".to_owned(),
                })
            })
        })
    };

    // B's agent_billing: succeeds.
    let b_agent_turn: AgentTurnFn = {
        let billing_c = b_billing_count.clone();
        Arc::new(move |req: AgentTurnRequest| {
            let node = req.node_id.clone();
            billing_c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(AgentTurnResult {
                    reply: format!("billing resolved at {node}"),
                    resolved: true,
                })
            })
        })
    };

    let exec_b = GraphExecutor::new(
        store.clone(),
        b_agent_turn,
        Arc::new(|_req: ToolCallRequest| {
            Box::pin(async move {
                Err(GraphExecError::Tool(
                    "tool should not be called in supervisor test".into(),
                ))
            })
        }),
        b_sup,
    );

    let outcome = exec_b
        .resume(&t, "sup-crash-run")
        .await
        .expect("resume must succeed");

    assert_eq!(
        outcome.status,
        RunStatus::Succeeded,
        "resumed supervisor run must complete with Succeeded"
    );

    // THE GUARANTEE: B's supervisor closure must NOT be called.
    // After phase 1, the cursor was already advanced to "agent_billing" (past
    // the supervisor node).  The drive loop in phase 2 resumes from
    // "agent_billing" — the supervisor node is never visited again.
    // The counter == 0 proves the decision was NOT re-computed.
    assert_eq!(
        b_sup_count.load(Ordering::SeqCst),
        0,
        "executor B supervisor must NOT be invoked (cursor already past supervisor; \
         decision survives in the durable cursor + ledger)"
    );

    // agent_billing must be invoked exactly once by B (the successful retry).
    assert_eq!(
        b_billing_count.load(Ordering::SeqCst),
        1,
        "executor B agent_billing must be invoked exactly once (successful retry)"
    );

    // Phase-2 trail: agent_billing + respond (2 entries).
    // The supervisor was visited in phase 1 only; phase 2 resumes from the
    // cursor that was already advanced past it.
    assert_eq!(
        outcome.trail.len(),
        2,
        "phase-2 trail must have 2 entries (agent_billing + respond): {:#?}",
        outcome.trail
    );

    let has_billing_agent = outcome
        .trail
        .iter()
        .any(|e| e["kind"] == "agent" && e["node"] == "agent_billing");
    assert!(
        has_billing_agent,
        "phase-2 trail must contain an agent_billing entry: {:#?}",
        outcome.trail
    );

    // Phase 2 must NOT contain agent_tech (the wrong branch).
    let has_tech_agent = outcome.trail.iter().any(|e| e["node"] == "agent_tech");
    assert!(
        !has_tech_agent,
        "phase-2 trail must NOT contain agent_tech (billing branch was taken): {:#?}",
        outcome.trail
    );

    // The reply must come from agent_billing (proves the billing branch was followed).
    assert!(
        outcome.reply.contains("billing resolved"),
        "final reply must come from agent_billing, got: {:?}",
        outcome.reply
    );

    // Extra: verify the ledger still holds the billing decision (the phase-1 record
    // persists across instances — it's what guarantees the billing cursor was saved).
    let ledger_decision = store
        .load_node_visit(&t, "sup-crash-run", "sup", 1)
        .await
        .expect("store accessible")
        .expect("supervisor/attempt-1 ledger entry must persist across instances");
    let decision: SupervisorResult =
        serde_json::from_value(ledger_decision).expect("ledger entry must deserialize");
    assert_eq!(
        decision.branch, "billing",
        "ledger still holds the billing decision after resume"
    );
}
