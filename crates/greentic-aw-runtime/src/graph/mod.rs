//! Durable agent-graph execution: model, router, executor, checkpointing.
//! Ported from the greentic-designer engine slice (PR #436); see
//! docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md.
pub mod model;

pub use model::{Edge, Graph, GraphConfig, GraphError, Node, NodeKind};
