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
    ToolCallRequest, ToolFn,
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
