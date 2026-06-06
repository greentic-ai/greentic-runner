//! Pure router evaluation: given a Router node and the current run state,
//! return the target node id of the outgoing edge to follow.
//!
//! Ported from the greentic-designer spike
//! (`src/orchestrate/agent_graph/router.rs`). Kept side-effect free and
//! deterministic so it is trivially testable and safe to replay on resume.

use super::model::{Graph, GraphError, NodeKind};
use super::state::GraphRunState;

/// Resolve the Router node's outgoing edge.
///
/// Returns the **target node id** selected by the branching logic:
/// - `"resolved"` branch when `state.resolved` is `true` OR
///   `state.iterations >= max_iterations`.
/// - `"loop"` branch otherwise.
///
/// # Errors
///
/// Returns [`GraphError::Invalid`] if:
/// - `router_id` does not name a node in `graph`.
/// - The named node is not a [`NodeKind::Router`].
/// - The required branch edge (`"loop"` or `"resolved"`) is absent.
///
/// These conditions are unreachable after a successful [`Graph::validate`]
/// but we never panic — always return `Err`.
pub fn route(graph: &Graph, router_id: &str, state: &GraphRunState) -> Result<String, GraphError> {
    let node = graph
        .node(router_id)
        .ok_or_else(|| GraphError::Invalid(format!("router '{router_id}' not found")))?;

    let max_iterations = match node.kind {
        NodeKind::Router { max_iterations } => max_iterations,
        _ => {
            return Err(GraphError::Invalid(format!(
                "node '{router_id}' is not a router"
            )));
        }
    };

    let branch = if state.resolved || state.iterations >= max_iterations {
        "resolved"
    } else {
        "loop"
    };

    graph
        .edges_from(router_id)
        .find(|e| e.branch.as_deref() == Some(branch))
        .map(|e| e.to.clone())
        .ok_or_else(|| GraphError::Invalid(format!("router '{router_id}' has no '{branch}' edge")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::graph::{GraphConfig, test_fixtures};

    #[test]
    fn routes_to_loop_when_unresolved_and_under_cap() {
        let cfg = GraphConfig::from_json(&test_fixtures::triage_json()).unwrap();
        let state = GraphRunState {
            iterations: 1,
            ..Default::default()
        };
        assert_eq!(route(&cfg.graph, "router", &state).unwrap(), "agent");
    }

    #[test]
    fn routes_to_resolved_when_flag_set() {
        let cfg = GraphConfig::from_json(&test_fixtures::triage_json()).unwrap();
        let state = GraphRunState {
            resolved: true,
            ..Default::default()
        };
        assert_eq!(route(&cfg.graph, "router", &state).unwrap(), "respond");
    }

    #[test]
    fn routes_to_resolved_at_iteration_cap() {
        // triage_json has maxIterations: 3
        let cfg = GraphConfig::from_json(&test_fixtures::triage_json()).unwrap();
        let state = GraphRunState {
            iterations: 3,
            ..Default::default()
        };
        assert_eq!(route(&cfg.graph, "router", &state).unwrap(), "respond");
    }

    #[test]
    fn non_router_node_returns_err() {
        let cfg = GraphConfig::from_json(&test_fixtures::triage_json()).unwrap();
        let state = GraphRunState::default();
        // "agent" is an Agent node, not a Router — must error
        let err = route(&cfg.graph, "agent", &state).unwrap_err();
        assert!(
            matches!(err, GraphError::Invalid(_)),
            "expected Invalid, got {err:?}"
        );
    }
}
