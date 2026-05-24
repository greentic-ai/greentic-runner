// placeholder — filled in subsequent tasks

use crate::{AgentError, AgentInput, AgentOutput, AgentRuntime, TenantContext};

/// Single iteration of the Plan-Act-Observe loop (Task 1.11).
///
/// This stub always returns an unimplemented error until Task 1.11 fills it in.
pub async fn run_step(
    _runtime: &AgentRuntime,
    _tenant: TenantContext,
    _session_id: &str,
    _agent_id: &str,
    _message: AgentInput,
) -> Result<AgentOutput, AgentError> {
    Err(AgentError::Internal("run_step not yet implemented".to_owned()))
}
