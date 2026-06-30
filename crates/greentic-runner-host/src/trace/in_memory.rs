//! In-memory execution-trace capture.
//!
//! The on-disk [`TraceRecorder`](super::TraceRecorder) is the production trace
//! sink, but interactive callers (e.g. the designer's live "Run Demo") need to
//! capture every intermediate node's INPUT and OUTPUT for a single activity
//! turn and read it back in-process. [`InMemoryObserver`] implements the
//! [`ExecutionObserver`] seam for exactly that: attach it to one flow execution,
//! run the turn, then [`InMemoryObserver::drain`] the collected
//! [`NodeTraceEntry`] list.
//!
//! [`FanOutObserver`] lets the caller-supplied observer coexist with the
//! existing on-disk `TraceRecorder`, so attaching an in-memory observer never
//! disables trace-to-disk.

use std::error::Error as StdError;
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::Value;

use crate::runner::engine::{ExecutionObserver, NodeEvent};

/// One captured node execution: the rendered INPUT, the raw OUTPUT (or error),
/// and the wall-clock duration. Serialised `camelCase` for the designer UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeTraceEntry {
    /// Flow-local node identifier.
    pub node_id: String,
    /// Backwards-compatible component label (`HostNode::component`).
    pub component: String,
    /// Operation/tool name, when the node carries one (empty otherwise).
    pub operation: String,
    /// Rendered input payload handed to the node (`on_node_start`).
    pub input: Value,
    /// Raw component output (`on_node_end`); `None` when the node errored.
    pub output: Option<Value>,
    /// Error string when the node failed (`on_node_error`); `None` on success.
    pub error: Option<String>,
    /// Wall-clock duration between start and end/error.
    pub duration_ms: u64,
}

/// A node whose `on_node_start` has fired but whose `on_node_end`/`on_node_error`
/// has not. Tracked as a LIFO stack so nested flow-call executions finalize the
/// correct entry.
struct PendingEntry {
    node_id: String,
    component: String,
    operation: String,
    input: Value,
    started: Instant,
}

#[derive(Default)]
struct ObserverState {
    pending: Vec<PendingEntry>,
    completed: Vec<NodeTraceEntry>,
}

/// Thread-safe [`ExecutionObserver`] that collects a [`NodeTraceEntry`] per
/// executed node. Entries are recorded in completion order (which equals
/// execution order for flat flows; nested flow-call nodes finalize inner nodes
/// before their outer node).
#[derive(Default)]
pub struct InMemoryObserver {
    state: Mutex<ObserverState>,
}

impl InMemoryObserver {
    /// Create an empty observer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the collected entries in completion order, clearing the buffer.
    pub fn drain(&self) -> Vec<NodeTraceEntry> {
        let mut state = self.state.lock();
        std::mem::take(&mut state.completed)
    }

    /// Return a clone of the collected entries without clearing the buffer.
    pub fn snapshot(&self) -> Vec<NodeTraceEntry> {
        self.state.lock().completed.clone()
    }

    fn finalize(&self, node_id: &str, output: Option<Value>, error: Option<String>) {
        let mut state = self.state.lock();
        // Pop the most recent pending entry for this node (LIFO handles nested
        // flow-call executions). Fall back to the last pending entry if the id
        // somehow does not match, so an entry is never silently dropped.
        let index = state
            .pending
            .iter()
            .rposition(|entry| entry.node_id == node_id)
            .or_else(|| state.pending.len().checked_sub(1));
        let Some(index) = index else {
            return;
        };
        let pending = state.pending.remove(index);
        let duration_ms = pending.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        state.completed.push(NodeTraceEntry {
            node_id: pending.node_id,
            component: pending.component,
            operation: pending.operation,
            input: pending.input,
            output,
            error,
            duration_ms,
        });
    }
}

impl ExecutionObserver for InMemoryObserver {
    fn on_node_start(&self, event: &NodeEvent<'_>) {
        let mut state = self.state.lock();
        state.pending.push(PendingEntry {
            node_id: event.node_id.to_string(),
            component: event.node.component.clone(),
            operation: event.node.operation_name().unwrap_or_default().to_string(),
            input: event.payload.clone(),
            started: Instant::now(),
        });
    }

    fn on_node_end(&self, event: &NodeEvent<'_>, output: &Value) {
        self.finalize(event.node_id, Some(output.clone()), None);
    }

    fn on_node_error(&self, event: &NodeEvent<'_>, error: &dyn StdError) {
        self.finalize(event.node_id, None, Some(error.to_string()));
    }
}

/// Fans every observer callback out to two observers so a caller-supplied
/// observer (e.g. [`InMemoryObserver`]) and the on-disk
/// [`TraceRecorder`](super::TraceRecorder) both receive every node event for one
/// activity turn.
pub struct FanOutObserver<'a> {
    primary: &'a dyn ExecutionObserver,
    secondary: &'a dyn ExecutionObserver,
}

impl<'a> FanOutObserver<'a> {
    /// Build a fan-out over two borrowed observers.
    pub fn new(primary: &'a dyn ExecutionObserver, secondary: &'a dyn ExecutionObserver) -> Self {
        Self { primary, secondary }
    }
}

impl ExecutionObserver for FanOutObserver<'_> {
    fn on_node_start(&self, event: &NodeEvent<'_>) {
        self.primary.on_node_start(event);
        self.secondary.on_node_start(event);
    }

    fn on_node_end(&self, event: &NodeEvent<'_>, output: &Value) {
        self.primary.on_node_end(event, output);
        self.secondary.on_node_end(event, output);
    }

    fn on_node_error(&self, event: &NodeEvent<'_>, error: &dyn StdError) {
        self.primary.on_node_error(event, error);
        self.secondary.on_node_error(event, error);
    }

    fn on_validation(&self, event: &NodeEvent<'_>, issues: &[crate::validate::ValidationIssue]) {
        self.primary.on_validation(event, issues);
        self.secondary.on_validation(event, issues);
    }
}
