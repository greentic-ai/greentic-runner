use greentic_runner_host::engine::GResult;
use greentic_runner_host::engine::runtime::{FlowResumeStore, IngressEnvelope};
use greentic_runner_host::runner::engine::{ExecutionState, FlowSnapshot, FlowWait};
use greentic_runner_host::storage::new_session_store;
use greentic_types::ReplyScope;
use serde_json::json;

fn envelope_for(conversation: &str) -> IngressEnvelope {
    IngressEnvelope {
        tenant: "demo".into(),
        env: Some("local".into()),
        pack_id: Some("pack.demo".into()),
        flow_id: "flow.main".into(),
        flow_type: Some("messaging".into()),
        action: Some("messaging".into()),
        session_hint: Some("demo:provider:chan:conv:user".into()),
        provider: Some("provider".into()),
        channel: Some(conversation.into()),
        conversation: Some(conversation.into()),
        user: Some("user".into()),
        activity_id: Some(format!("activity-{conversation}")),
        timestamp: None,
        payload: json!({ "text": "hi" }),
        metadata: None,
        reply_scope: Some(ReplyScope {
            conversation: conversation.into(),
            thread: None,
            reply_to: None,
            correlation: None,
        }),
    }
    .canonicalize()
}

fn wait_for(next_node: &str) -> FlowWait {
    let state: ExecutionState = serde_json::from_value(json!({
        "input": { "text": "hi" },
        "nodes": {},
        "egress": []
    }))
    .expect("state");
    FlowWait {
        reason: Some("await-user".into()),
        snapshot: FlowSnapshot {
            pack_id: "pack.demo".into(),
            flow_id: "flow.main".into(),
            next_flow: None,
            next_node: next_node.into(),
            state,
        },
    }
}

#[test]
fn reply_scope_routes_waits_independently() -> GResult<()> {
    let store = FlowResumeStore::new(new_session_store());
    let envelope_a = envelope_for("conv-a");
    let envelope_b = envelope_for("conv-b");

    let wait_a = wait_for("node-a");
    let wait_b = wait_for("node-b");

    let _ = store.save(&envelope_a, &wait_a)?;
    let _ = store.save(&envelope_b, &wait_b)?;

    let snapshot_a = store.fetch(&envelope_a)?.expect("snapshot A missing");
    let snapshot_b = store.fetch(&envelope_b)?.expect("snapshot B missing");

    assert_eq!(snapshot_a.next_node, "node-a");
    assert_eq!(snapshot_b.next_node, "node-b");

    store.clear(&envelope_a)?;
    store.clear(&envelope_b)?;
    Ok(())
}
