//! Durable agent-graph execution: model, router, executor, checkpointing.
//! Ported from the greentic-designer engine slice (PR #436); see
//! docs/superpowers/specs/2026-06-06-runtime-agent-graph-execution-design.md.
pub mod checkpoint;
pub mod executor;
pub mod model;
pub mod redis_checkpoint;
pub mod router;
pub mod state;

#[cfg(test)]
pub(crate) mod test_fixtures;

pub use checkpoint::{
    CheckpointError, CheckpointStore, GraphRunRecord, InMemoryCheckpointStore, NodeVisitOutcome,
    RunStatus,
};
pub use executor::{
    AgentTurnFn, AgentTurnRequest, AgentTurnResult, BoxFut, GraphExecError, GraphExecutor,
    GraphRunOutcome, ToolCallRequest, ToolFn,
};
pub use model::{Edge, Graph, GraphConfig, GraphError, Node, NodeKind};
pub use redis_checkpoint::RedisCheckpointStore;
pub use router::route;
pub use state::{GraphMessage, GraphRole, GraphRunState};
