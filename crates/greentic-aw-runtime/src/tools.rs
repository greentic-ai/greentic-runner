//! Tool resolution + dispatch helpers. Phase 1 introduces only the
//! type surface so the loop scaffold compiles; Phase 3 wires
//! `ExtensionRuntime::invoke_tool` via `spawn_blocking` and the
//! Redis-backed idempotency ledger.

use crate::config::ToolRef;
use crate::llm::LlmToolSchema;

/// Convert a vector of allowed [`ToolRef`]s into [`LlmToolSchema`]
/// entries the LLM understands. Phase 3 replaces this stub with a
/// real call to `ExtensionRuntime::list_tools`.
pub fn list_tools_for_llm(allowed: &[ToolRef]) -> Vec<LlmToolSchema> {
    allowed
        .iter()
        .map(|t| LlmToolSchema {
            extension_id: t.extension_id.clone(),
            tool_name: t.tool_name.clone(),
            description: String::new(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_tools_for_llm_maps_one_to_one() {
        let allowed = vec![
            ToolRef {
                extension_id: "http".into(),
                tool_name: "fetch".into(),
            },
            ToolRef {
                extension_id: "calendar".into(),
                tool_name: "create".into(),
            },
        ];
        let schemas = list_tools_for_llm(&allowed);
        assert_eq!(schemas.len(), 2);
        assert_eq!(schemas[0].tool_name, "fetch");
        assert_eq!(schemas[1].extension_id, "calendar");
    }
}
