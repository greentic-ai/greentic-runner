//! `RunOptions::entry_node` — start a run at a named node.
//!
//! Card-driven messaging packs name the next node to run on the inbound
//! activity. Without this option a host can only restart the flow at its
//! entrypoint every turn, so the nodes chained between two cards never execute.
//!
//! The `runner-components` fixture flow is `qa` (component.exec) -> `emit`
//! (emit.response). That is enough to tell the two entries apart: only a run
//! that honours `entry_node: "emit"` skips `qa`.
//!
//! Note the fixture must be the built `.gtpack`, not the source directory —
//! running the directory verifies nothing and executes no flow nodes at all.

use greentic_runner_desktop::{RunOptions, RunResult, TenantContext, run_pack_with_options};
use serde_json::json;
use tempfile::TempDir;

fn fixture_pack() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/packs/runner-components/runner-components.gtpack")
}

fn executed_nodes(result: &RunResult) -> Vec<String> {
    result
        .node_summaries
        .iter()
        .map(|n| n.node_id.clone())
        .collect()
}

fn run(entry_node: Option<&str>) -> RunResult {
    run_pack_with_options(
        fixture_pack(),
        RunOptions {
            entry_flow: Some("demo.flow".to_string()),
            entry_node: entry_node.map(str::to_string),
            input: json!({}),
            ..RunOptions::default()
        },
    )
    .expect("run should not fail to start")
}

fn run_with_session(entry_node: Option<&str>, session_dir: &std::path::Path) -> RunResult {
    run_pack_with_options(
        fixture_pack(),
        RunOptions {
            entry_flow: Some("demo.flow".to_string()),
            entry_node: entry_node.map(str::to_string),
            input: json!({}),
            ctx: TenantContext {
                session_id: Some("sess-precedence".to_string()),
                ..TenantContext::default_local()
            },
            session_state_dir: Some(session_dir.to_path_buf()),
            ..RunOptions::default()
        },
    )
    .expect("run should not fail to start")
}

fn snapshot_files(session_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(session_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(".snapshot.json"))
                .collect()
        })
        .unwrap_or_default()
}

/// Plant a resume snapshot that would send the run to `emit`, skipping `qa`.
fn plant_snapshot(session_dir: &std::path::Path, pack_id: &str) {
    let snapshot = json!({
        "pack_id": pack_id,
        "flow_id": "demo.flow",
        "next_node": "emit",
        "state": {
            "entry": {}, "input": {}, "nodes": {}, "egress": [], "redirect_count": 0
        }
    });
    std::fs::write(
        session_dir.join("sess-precedence.snapshot.json"),
        serde_json::to_vec_pretty(&snapshot).expect("encode snapshot"),
    )
    .expect("write snapshot");
}

/// A parked snapshot must NOT hijack an explicit entry node.
///
/// Card journeys park on every rendered card, so a snapshot is present on all
/// but the first turn. Letting it win means the run resumes where the previous
/// turn stopped and ignores the button the user just pressed — which silently
/// pins the journey to one card. Getting this precedence backwards is exactly
/// how the meridian quote journey stalled on page 2 forever.
#[test]
fn entry_node_outranks_a_parked_resume_snapshot() {
    let tmp = TempDir::new().expect("tempdir");
    let session_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let pack_id = run(None).pack_id;

    // Premise: prove the planted snapshot is actually loaded and honoured.
    // Without this the precedence assertion below could pass simply because the
    // snapshot was malformed and silently ignored.
    plant_snapshot(&session_dir, &pack_id);
    let resumed = executed_nodes(&run_with_session(None, &session_dir));
    assert!(
        resumed.iter().any(|n| n == "emit") && !resumed.iter().any(|n| n == "qa"),
        "planted snapshot was not honoured, so this test cannot prove anything \
         about precedence — got {resumed:?}"
    );

    // Now the real assertion: the same snapshot must lose to an entry node that
    // names a node UPSTREAM of it. Resuming would never reach `qa`.
    plant_snapshot(&session_dir, &pack_id);
    let jumped = executed_nodes(&run_with_session(Some("qa"), &session_dir));
    assert!(
        jumped.iter().any(|n| n == "qa"),
        "the explicit entry node must win over the parked snapshot, got {jumped:?}"
    );

    // And the stale snapshot must be gone, or the next turn resumes into a node
    // this run has already moved past.
    assert!(
        snapshot_files(&session_dir).is_empty(),
        "an entry-node run must clear the snapshot it overrode"
    );
}

/// Baseline that makes the `entry_node` assertion below discriminating: with no
/// `entry_node` the run starts at the flow's declared entrypoint, so `qa`
/// executes. If this ever stopped executing `qa`, the assertion that
/// `entry_node` SKIPS it would pass for the wrong reason.
#[test]
fn without_entry_node_the_run_starts_at_the_entrypoint() {
    let nodes = executed_nodes(&run(None));
    assert!(
        nodes.iter().any(|n| n == "qa"),
        "entrypoint run must execute the entry node `qa`, got {nodes:?}"
    );
}

#[test]
fn entry_node_starts_the_run_at_the_named_node() {
    let nodes = executed_nodes(&run(Some("emit")));
    assert!(
        nodes.iter().any(|n| n == "emit"),
        "run must execute the named entry node `emit`, got {nodes:?}"
    );
    assert!(
        !nodes.iter().any(|n| n == "qa"),
        "`qa` is upstream of `emit` and must NOT run, got {nodes:?}"
    );
}

/// The option must not silently degrade to an entrypoint restart when the node
/// does not exist — that silent fallback is the bug this option removes.
#[test]
fn unknown_entry_node_fails_rather_than_restarting_at_the_entrypoint() {
    let result = run_pack_with_options(
        fixture_pack(),
        RunOptions {
            entry_flow: Some("demo.flow".to_string()),
            entry_node: Some("no_such_node".to_string()),
            input: json!({}),
            ..RunOptions::default()
        },
    );

    match result {
        Err(err) => {
            let msg = format!("{err:#}");
            assert!(
                msg.contains("no_such_node"),
                "error should name the missing node, got {msg}"
            );
        }
        Ok(res) => {
            let nodes = executed_nodes(&res);
            assert!(
                !nodes.iter().any(|n| n == "qa"),
                "an unknown entry node must never fall back to the entrypoint, got {nodes:?}"
            );
            assert_ne!(
                res.status,
                greentic_runner_desktop::RunStatus::Success,
                "an unknown entry node must not report success, got {res:?}"
            );
        }
    }
}
