//! Shared in-memory test fixtures for graph unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Returns the JSON string for a minimal triage-style graph used across the
/// graph module tests. The graph has four nodes (agent, lookup tool, router,
/// respond) and maxIterations=3 on the router.
pub(crate) fn triage_json() -> String {
    serde_json::json!({
        "schemaVersion": 1,
        "entry": "agent",
        "nodes": [
            {"id": "agent", "kind": "agent", "systemPrompt": "You triage.", "model": "gpt-4o-mini", "tools": []},
            {"id": "lookup", "kind": "tool", "toolName": "kb.search"},
            {"id": "router", "kind": "router", "maxIterations": 3},
            {"id": "respond", "kind": "respond"}
        ],
        "edges": [
            {"from": "agent", "to": "lookup"},
            {"from": "lookup", "to": "router"},
            {"from": "router", "to": "agent", "branch": "loop"},
            {"from": "router", "to": "respond", "branch": "resolved"}
        ]
    })
    .to_string()
}
