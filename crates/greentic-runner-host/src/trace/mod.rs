#[cfg(feature = "agentic-worker")]
pub mod agent_audit;
pub mod audit_event;
pub mod audit_sink;
mod model;
mod recorder;

pub use model::{TraceEnvelope, TraceError, TraceFlow, TraceHash, TracePack, TraceStep};
pub(crate) use recorder::generate_audit_event_id;
pub use recorder::{PackTraceInfo, TraceConfig, TraceContext, TraceMode, TraceRecorder};
