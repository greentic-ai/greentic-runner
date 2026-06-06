//! Data model for durable agent-graph execution.
//!
//! Ported from the greentic-designer engine spike (PR #436,
//! `src/orchestrate/agent_graph/model.rs`) and extended with a
//! schema-versioned `GraphConfig` envelope for the runner sidecar wire
//! format.
//!
//! See `docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md`
//! for the full design.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Node kinds
// ---------------------------------------------------------------------------

/// The role a node plays in the graph, plus its per-kind configuration.
///
/// Serialised with `"kind"` as the tag field; variant names are lowercase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum NodeKind {
    /// An LLM agent that may invoke tools.
    #[serde(rename_all = "camelCase")]
    Agent {
        system_prompt: String,
        model: String,
        #[serde(default)]
        tools: Vec<String>,
    },
    /// A deterministic tool call node.
    #[serde(rename_all = "camelCase")]
    Tool { tool_name: String },
    /// A routing decision node (loop vs. resolved).
    #[serde(rename_all = "camelCase")]
    Router {
        #[serde(default = "default_max_iterations")]
        max_iterations: u32,
    },
    /// Terminal node that emits the final reply.
    Respond,
}

fn default_max_iterations() -> u32 {
    4
}

// ---------------------------------------------------------------------------
// Graph primitives
// ---------------------------------------------------------------------------

/// A single node in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// A directed edge between two nodes. `branch` discriminates router exits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// Router uses this to pick an outgoing edge (`"loop"` | `"resolved"`).
    #[serde(default)]
    pub branch: Option<String>,
}

// ---------------------------------------------------------------------------
// Graph (bare, no schema-version envelope)
// ---------------------------------------------------------------------------

/// The bare execution graph: an entry node, a node list, and an edge list.
///
/// Deserialised via `serde` and validated via [`Graph::validate`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Graph {
    pub entry: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Graph {
    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Outgoing edges for a node, in declaration order.
    pub fn edges_from(&self, id: &str) -> impl Iterator<Item = &Edge> {
        self.edges.iter().filter(move |e| e.from == id)
    }

    /// Validate the graph against the five structural rules:
    ///
    /// 1. `entry` names an existing node.
    /// 2. Every edge endpoint names an existing node.
    /// 3. Agent and Tool nodes have exactly one outgoing edge.
    /// 4. Router nodes have both a `"loop"` and a `"resolved"` branch edge.
    /// 5. Respond nodes have zero outgoing edges.
    pub fn validate(&self) -> Result<(), String> {
        // Rule 1: entry exists.
        if self.node(&self.entry).is_none() {
            return Err(format!("entry node '{}' not found", self.entry));
        }

        // Rule 2: all edge endpoints exist.
        for e in &self.edges {
            if self.node(&e.from).is_none() {
                return Err(format!("edge from unknown node '{}'", e.from));
            }
            if self.node(&e.to).is_none() {
                return Err(format!("edge to unknown node '{}'", e.to));
            }
        }

        // Rules 3–5: per-kind outgoing-edge constraints.
        for node in &self.nodes {
            let out: Vec<&Edge> = self.edges_from(&node.id).collect();
            match &node.kind {
                NodeKind::Agent { .. } | NodeKind::Tool { .. } => {
                    if out.len() != 1 {
                        return Err(format!(
                            "node '{}' must have exactly 1 outgoing edge, found {}",
                            node.id,
                            out.len()
                        ));
                    }
                }
                NodeKind::Router { .. } => {
                    let has = |b: &str| out.iter().any(|e| e.branch.as_deref() == Some(b));
                    if !has("loop") || !has("resolved") {
                        return Err(format!(
                            "router '{}' must have both a 'loop' and a 'resolved' branch edge",
                            node.id
                        ));
                    }
                }
                NodeKind::Respond => {
                    if !out.is_empty() {
                        return Err(format!(
                            "respond node '{}' cannot have outgoing edges",
                            node.id
                        ));
                    }
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GraphConfig — schema-versioned envelope
// ---------------------------------------------------------------------------

/// Wire envelope for an agent-graph document (`agent-graph.json` sidecar or
/// the admin-registry graph document). `schema_version` gates forward
/// evolution; currently only version 1 is accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphConfig {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(flatten)]
    pub graph: Graph,
}

fn default_schema_version() -> u32 {
    1
}

/// The only `schema_version` value this build accepts.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// GraphError
// ---------------------------------------------------------------------------

/// Errors produced when parsing or validating a [`GraphConfig`].
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error("invalid graph: {0}")]
    Invalid(String),
    #[error("graph JSON parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("unsupported graph schemaVersion {0}")]
    UnsupportedSchemaVersion(u32),
}

// ---------------------------------------------------------------------------
// GraphConfig impl
// ---------------------------------------------------------------------------

impl GraphConfig {
    /// Parse and validate a JSON string into a [`GraphConfig`].
    ///
    /// Returns [`GraphError::UnsupportedSchemaVersion`] when `schema_version`
    /// is not equal to [`SUPPORTED_SCHEMA_VERSION`], and
    /// [`GraphError::Invalid`] when the graph fails structural validation.
    pub fn from_json(raw: &str) -> Result<Self, GraphError> {
        let cfg: GraphConfig = serde_json::from_str(raw)?;
        if cfg.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(GraphError::UnsupportedSchemaVersion(cfg.schema_version));
        }
        cfg.graph.validate().map_err(GraphError::Invalid)?;
        Ok(cfg)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn triage_graph_json() -> serde_json::Value {
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
    }

    #[test]
    fn parses_and_validates_triage_graph() {
        let cfg = GraphConfig::from_json(&triage_graph_json().to_string()).expect("valid graph");
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.graph.entry, "agent");
        assert_eq!(cfg.graph.nodes.len(), 4);
    }

    #[test]
    fn rejects_unknown_entry() {
        let mut v = triage_graph_json();
        v["entry"] = "missing".into();
        let err = GraphConfig::from_json(&v.to_string()).unwrap_err();
        assert!(matches!(err, GraphError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn rejects_router_without_resolved_branch() {
        let mut v = triage_graph_json();
        v["edges"]
            .as_array_mut()
            .unwrap()
            .retain(|e| e["branch"] != "resolved");
        assert!(GraphConfig::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn rejects_agent_with_two_outgoing_edges() {
        let mut v = triage_graph_json();
        v["edges"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"from": "agent", "to": "router"}));
        assert!(GraphConfig::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn rejects_respond_with_outgoing_edge() {
        let mut v = triage_graph_json();
        v["edges"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"from": "respond", "to": "agent"}));
        assert!(GraphConfig::from_json(&v.to_string()).is_err());
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let mut v = triage_graph_json();
        v["schemaVersion"] = 2.into();
        let err = GraphConfig::from_json(&v.to_string()).unwrap_err();
        assert!(matches!(err, GraphError::UnsupportedSchemaVersion(2)));
    }
}
