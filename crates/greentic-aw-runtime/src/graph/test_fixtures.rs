//! Shared in-memory test fixtures for graph unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Returns the JSON string for a minimal triage-style graph used across the
/// graph module tests. The graph has four nodes (agent, lookup tool, router,
/// respond) and maxIterations=3 on the router.
///
/// This is a **schemaVersion 1** graph; it must remain parseable after v2
/// changes.
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

/// Returns the JSON string for a **schemaVersion 2** supervisor graph.
///
/// Topology (simple, valid):
///
/// ```
/// sup (supervisor: routes=[billing, tech])
///  ├─[billing]─► agent_billing ─► router_billing ─┬─[loop]──► sup (back)
///  │                                               └─[resolved]─► respond
///  └─[tech]────► agent_tech ───► router_tech    ─┬─[loop]──► sup (back)
///                                                 └─[resolved]─► respond
/// ```
///
/// Both branches share the same `respond` terminal.
/// Routing back to `sup` is legal (supervisor does not count as a
/// respond-inside-branch; loops are allowed on supervisor nodes).
pub(crate) fn supervisor_json() -> String {
    serde_json::json!({
        "schemaVersion": 2,
        "entry": "sup",
        "nodes": [
            {
                "id": "sup",
                "kind": "supervisor",
                "systemPrompt": "Route the request to the correct specialist.",
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
                "id": "router_billing",
                "kind": "router",
                "maxIterations": 2
            },
            {
                "id": "agent_tech",
                "kind": "agent",
                "systemPrompt": "You handle technical issues.",
                "model": "gpt-4o-mini",
                "tools": []
            },
            {
                "id": "router_tech",
                "kind": "router",
                "maxIterations": 2
            },
            {
                "id": "respond",
                "kind": "respond"
            }
        ],
        "edges": [
            // Supervisor picks a branch.
            {"from": "sup",           "to": "agent_billing", "branch": "billing"},
            {"from": "sup",           "to": "agent_tech",    "branch": "tech"},
            // Billing branch.
            {"from": "agent_billing", "to": "router_billing"},
            {"from": "router_billing","to": "agent_billing",  "branch": "loop"},
            {"from": "router_billing","to": "respond",        "branch": "resolved"},
            // Tech branch.
            {"from": "agent_tech",    "to": "router_tech"},
            {"from": "router_tech",   "to": "agent_tech",     "branch": "loop"},
            {"from": "router_tech",   "to": "respond",        "branch": "resolved"}
        ]
    })
    .to_string()
}

/// Returns the JSON string for a **schemaVersion 2** parallel/join graph.
///
/// Topology:
///
/// ```
/// entry (agent) ──► fan (parallel) ─[a]─► agent_a ─┐
///                                  ─[b]─► tool_b  ─┤
///                                                    ▼
///                                              meet (join) ──► respond
/// ```
pub(crate) fn parallel_json() -> String {
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
            {"id": "fan",   "kind": "parallel"},
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
