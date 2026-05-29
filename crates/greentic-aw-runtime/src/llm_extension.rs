//! `ExtensionLlmBackend` — runs the worker LLM through an installed extension
//! instead of a hardcoded provider client (spec: LLM provider as an extension).
//!
//! Approach B: the LLM-bridge extension exposes a tool named `complete` whose
//! args JSON is a [`BridgeRequest`] (system prompt + history + tool schemas +
//! the host-resolved credential) and whose result JSON is the existing
//! [`LlmResponse`]. Dispatch goes through the generic
//! `ExtensionRuntime::invoke_tool` (sync → `spawn_blocking`), abstracted behind
//! [`LlmExtensionInvoker`] so the backend is unit-testable without a real WASM
//! component (`invoke_tool` requires a loaded component; `for_test()` has none).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Serialize;

use crate::error::LlmError;
use crate::llm::{LlmBackend, LlmRequest, LlmResponse, LlmToolSchema};
use crate::state::ChatMessage;

/// The tool an LLM-bridge extension exposes (approach B).
pub const LLM_COMPLETE_TOOL: &str = "complete";

/// Host-resolved credential passed to the bridge per call (spec Decision 1:
/// the host resolves creds and passes them in the request; the extension is a
/// stateless bridge). `secret_ref` is reserved for a future hardening path.
#[derive(Clone, Debug, Serialize)]
pub struct BridgeCredential {
    pub provider: String,
    pub model: String,
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// The wire payload sent to the bridge's `complete` tool.
#[derive(Debug, Serialize)]
struct BridgeRequest<'a> {
    system_prompt: &'a str,
    history: &'a [ChatMessage],
    tools: &'a [LlmToolSchema],
    credential: &'a BridgeCredential,
}

/// Synchronous dispatch seam. Prod impl calls `ExtensionRuntime::invoke_tool`;
/// tests script the JSON response without a WASM component.
pub trait LlmExtensionInvoker: Send + Sync {
    fn invoke(&self, extension_id: &str, tool: &str, args_json: &str) -> Result<String, String>;
}

/// Production invoker over a loaded `ExtensionRuntime`.
pub struct RuntimeInvoker {
    pub ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
}

impl LlmExtensionInvoker for RuntimeInvoker {
    fn invoke(&self, extension_id: &str, tool: &str, args_json: &str) -> Result<String, String> {
        self.ext_runtime
            .invoke_tool(extension_id, tool, args_json)
            .map_err(|e| e.to_string())
    }
}

/// `LlmBackend` that delegates to an LLM-bridge extension.
pub struct ExtensionLlmBackend {
    invoker: Arc<dyn LlmExtensionInvoker>,
    extension_id: String,
    credential: BridgeCredential,
}

impl ExtensionLlmBackend {
    /// Build over a real `ExtensionRuntime`.
    pub fn new(
        ext_runtime: Arc<greentic_ext_runtime::ExtensionRuntime>,
        extension_id: impl Into<String>,
        credential: BridgeCredential,
    ) -> Self {
        Self {
            invoker: Arc::new(RuntimeInvoker { ext_runtime }),
            extension_id: extension_id.into(),
            credential,
        }
    }

    /// Build over an arbitrary invoker (tests).
    pub fn with_invoker(
        invoker: Arc<dyn LlmExtensionInvoker>,
        extension_id: impl Into<String>,
        credential: BridgeCredential,
    ) -> Self {
        Self {
            invoker,
            extension_id: extension_id.into(),
            credential,
        }
    }
}

impl LlmBackend for ExtensionLlmBackend {
    fn complete<'a>(
        &'a self,
        request: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        let invoker = self.invoker.clone();
        let ext_id = self.extension_id.clone();
        let credential = self.credential.clone();
        Box::pin(async move {
            let payload = BridgeRequest {
                system_prompt: &request.system_prompt,
                history: &request.history,
                tools: &request.tools,
                credential: &credential,
            };
            let args_json = serde_json::to_string(&payload)
                .map_err(|e| LlmError::BadRequest(format!("encode bridge request: {e}")))?;
            let raw = tokio::task::spawn_blocking(move || {
                invoker.invoke(&ext_id, LLM_COMPLETE_TOOL, &args_json)
            })
            .await
            .map_err(|e| LlmError::Transport(format!("llm bridge join: {e}")))?
            // A failed dispatch (extension missing / WASM trap) is a config-class
            // error, not a transient 5xx — surface as BadRequest so the retry
            // decorator does NOT loop on it.
            .map_err(LlmError::BadRequest)?;
            serde_json::from_str::<LlmResponse>(&raw)
                .map_err(|e| LlmError::Decode(format!("decode bridge response: {e}")))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::LlmProviderRef;
    use crate::state::ToolCallRecord;
    use std::sync::Mutex;

    fn cred() -> BridgeCredential {
        BridgeCredential {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_key: "sk-test".into(),
            base_url: None,
        }
    }

    fn req() -> LlmRequest {
        LlmRequest {
            system_prompt: "be helpful".into(),
            history: vec![ChatMessage::User { content: "hi".into() }],
            tools: vec![LlmToolSchema {
                extension_id: "http".into(),
                tool_name: "fetch".into(),
                description: "fetch".into(),
                parameters: serde_json::json!({ "type": "object" }),
            }],
            provider: LlmProviderRef { provider: "openai".into(), model: "gpt-4o".into() },
        }
    }

    /// Captures the args JSON it was handed and returns a scripted reply.
    struct ScriptInvoker {
        seen: Mutex<Option<(String, String, String)>>,
        reply: Result<String, String>,
    }
    impl LlmExtensionInvoker for ScriptInvoker {
        fn invoke(&self, ext: &str, tool: &str, args: &str) -> Result<String, String> {
            *self.seen.lock().unwrap() = Some((ext.into(), tool.into(), args.into()));
            self.reply.clone()
        }
    }

    #[tokio::test]
    async fn sends_complete_tool_with_credential_and_parses_response() {
        let reply = serde_json::json!({
            "content": "done",
            "tool_calls": [{
                "call_id": "c1", "extension_id": "http", "tool_name": "fetch",
                "args": { "url": "x" }
            }],
            "tokens_in": 3, "tokens_out": 5
        })
        .to_string();
        let inv = Arc::new(ScriptInvoker { seen: Mutex::new(None), reply: Ok(reply) });
        let backend = ExtensionLlmBackend::with_invoker(inv.clone(), "llm-openai-bridge", cred());

        let resp = backend.complete(req()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("done"));
        assert_eq!(resp.tool_calls.len(), 1);
        let tc: &ToolCallRecord = &resp.tool_calls[0];
        assert_eq!(tc.extension_id, "http");
        assert_eq!(tc.tool_name, "fetch");

        // Dispatched to the right extension + tool, and the credential + tools
        // schema rode along in the args.
        let (ext, tool, args) = inv.seen.lock().unwrap().clone().unwrap();
        assert_eq!(ext, "llm-openai-bridge");
        assert_eq!(tool, "complete");
        let v: serde_json::Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["credential"]["api_key"], "sk-test");
        assert_eq!(v["credential"]["provider"], "openai");
        assert_eq!(v["system_prompt"], "be helpful");
        assert_eq!(v["tools"][0]["tool_name"], "fetch");
        // base_url omitted when None.
        assert!(v["credential"].get("base_url").is_none());
    }

    #[tokio::test]
    async fn invoker_error_maps_to_bad_request() {
        let inv = Arc::new(ScriptInvoker {
            seen: Mutex::new(None),
            reply: Err("NotFound(llm-openai-bridge)".into()),
        });
        let backend = ExtensionLlmBackend::with_invoker(inv, "llm-openai-bridge", cred());
        let err = backend.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::BadRequest(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn malformed_reply_maps_to_decode() {
        let inv = Arc::new(ScriptInvoker {
            seen: Mutex::new(None),
            reply: Ok("not json".into()),
        });
        let backend = ExtensionLlmBackend::with_invoker(inv, "llm-openai-bridge", cred());
        let err = backend.complete(req()).await.unwrap_err();
        assert!(matches!(err, LlmError::Decode(_)), "got {err:?}");
    }
}
