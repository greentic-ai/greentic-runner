//! Durable agent-graph execution: model, router, executor, checkpointing.
//! Ported from the greentic-designer engine slice (PR #436); see
//! docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md.
pub mod model;
pub mod router;
pub mod state;

#[cfg(test)]
pub(crate) mod test_fixtures;

pub use model::{Edge, Graph, GraphConfig, GraphError, Node, NodeKind};
pub use router::route;
pub use state::{GraphMessage, GraphRole, GraphRunState};
