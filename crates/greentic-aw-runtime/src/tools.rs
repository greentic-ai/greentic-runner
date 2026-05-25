//! Tool resolution + dispatch helpers.
//!
//! `ExtensionRuntime::invoke_tool` is a synchronous `fn` performing
//! Wasmtime WASM dispatch (CPU-bound, may block seconds). Every call
//! site MUST wrap it in `tokio::task::spawn_blocking` (spec §5.3) — this
//! module is the ONLY place that calls `invoke_tool`.
//!
//! Each tool call is recorded in Redis by `tool_call_id` BEFORE the
//! result is committed so a state-save failure cannot cause a
//! double-dispatch on the next `step()` (idempotency ledger).

use std::sync::Arc;

use greentic_ext_runtime::ExtensionRuntime;
use serde::{Deserialize, Serialize};

use crate::config::ToolRef;
use crate::error::AgentError;
use crate::llm::LlmToolSchema;
use crate::state::ToolCallRecord;
use crate::tenant::TenantContext;

/// Whether the agent may call this tool — exact (extension_id, tool_name) match.
pub fn is_tool_allowed(call: &ToolCallRecord, allowed: &[ToolRef]) -> bool {
    allowed
        .iter()
        .any(|t| t.extension_id == call.extension_id && t.tool_name == call.tool_name)
}

/// Map allow-listed tools to LLM-facing schemas via
/// `ExtensionRuntime::list_tools`. Tools whose extension is not loaded,
/// or that the extension doesn't expose, are logged and skipped (the
/// LLM simply won't see them). `input_schema_json` is parsed into a
/// JSON Value for the LLM tool parameters; on parse failure an empty
/// object schema is used.
pub fn list_tools_for_llm(
    ext_runtime: &ExtensionRuntime,
    allowed: &[ToolRef],
) -> Vec<LlmToolSchema> {
    let mut out = Vec::with_capacity(allowed.len());
    for t in allowed {
        match ext_runtime.list_tools(&t.extension_id) {
            Ok(defs) => {
                if let Some(def) = defs.into_iter().find(|d| d.name == t.tool_name) {
                    let parameters: serde_json::Value = serde_json::from_str(
                        &def.input_schema_json,
                    )
                    .unwrap_or_else(|_| serde_json::json!({"type": "object", "properties": {}}));
                    out.push(LlmToolSchema {
                        extension_id: t.extension_id.clone(),
                        tool_name: t.tool_name.clone(),
                        description: def.description,
                        parameters,
                    });
                } else {
                    tracing::warn!(
                        extension = %t.extension_id, tool = %t.tool_name,
                        "tool not found in extension; dropping from LLM tool list"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    extension = %t.extension_id, error = %e,
                    "extension list_tools failed; skipping"
                );
            }
        }
    }
    out
}

/// Dispatch a single tool call. Wraps the blocking `invoke_tool` in
/// `tokio::task::spawn_blocking` so the async executor thread is never
/// stalled. Returns the tool result as a JSON Value.
pub async fn dispatch_tool_call(
    ext_runtime: Arc<ExtensionRuntime>,
    call: ToolCallRecord,
) -> Result<serde_json::Value, AgentError> {
    let args_json = call.args.to_string();
    let extension_id = call.extension_id.clone();
    let tool_name = call.tool_name.clone();
    let raw = tokio::task::spawn_blocking(move || {
        ext_runtime.invoke_tool(&extension_id, &tool_name, &args_json)
    })
    .await
    .map_err(|e| AgentError::ToolDispatch(format!("join: {e}")))?
    .map_err(|e| AgentError::ToolDispatch(format!("invoke: {e}")))?;
    serde_json::from_str(&raw).map_err(|e| AgentError::ToolDispatch(format!("decode: {e}")))
}

/// Idempotency ledger entry stored under
/// `aw:{tenant}:{env}:{session}:tool_calls:{call_id}` (TTL 7 days).
#[derive(Serialize, Deserialize, Clone)]
pub struct ToolLedgerEntry {
    pub result: serde_json::Value,
}

pub fn ledger_key(tenant: &TenantContext, session_id: &str, call_id: &str) -> String {
    format!("{}:{session_id}:tool_calls:{call_id}", tenant.key_prefix())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_tool_allowed_returns_true_for_exact_match() {
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
        }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "fetch".into(),
            args: serde_json::json!({}),
        };
        assert!(is_tool_allowed(&call, &allowed));
    }

    #[test]
    fn is_tool_allowed_returns_false_for_unauthorized_tool() {
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
        }];
        let call = ToolCallRecord {
            call_id: "c1".into(),
            extension_id: "http".into(),
            tool_name: "post".into(),
            args: serde_json::json!({}),
        };
        assert!(!is_tool_allowed(&call, &allowed));
    }

    #[test]
    fn ledger_key_includes_tenant_env_session_callid() {
        let tc = TenantContext::new("acme", "prod");
        let key = ledger_key(&tc, "sess-1", "call-abc");
        assert_eq!(key, "aw:acme:prod:sess-1:tool_calls:call-abc");
    }

    #[test]
    fn list_tools_for_llm_with_no_extensions_returns_empty() {
        // for_test runtime has no extensions loaded → list_tools errors
        // (NotFound) for every ext → all skipped → empty result.
        let rt = ExtensionRuntime::for_test();
        let allowed = vec![ToolRef {
            extension_id: "http".into(),
            tool_name: "fetch".into(),
        }];
        let schemas = list_tools_for_llm(&rt, &allowed);
        assert!(schemas.is_empty());
    }
}
