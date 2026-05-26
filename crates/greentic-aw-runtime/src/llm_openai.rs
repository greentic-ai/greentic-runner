//! OpenAI Chat Completions API client (function-calling mode).
//! Single backend MVP; multi-provider routing deferred (spec §10).

use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::LlmError;
use crate::llm::{LlmBackend, LlmRequest, LlmResponse, LlmToolSchema};
use crate::state::{ChatMessage, ToolCallRecord};

pub struct OpenAiLlmBackend {
    api_key: String,
    base_url: String,
    client: Client,
}

impl OpenAiLlmBackend {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, "https://api.openai.com")
    }

    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            client: Client::builder()
                .timeout(Duration::from_secs(45))
                .build()
                .unwrap_or_else(|_| Client::new()),
        }
    }
}

#[derive(Serialize)]
struct OaRequest<'a> {
    model: &'a str,
    messages: Vec<OaMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OaTool<'a>>>,
    tool_choice: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum OaMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<OaToolCallEmit>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Serialize)]
struct OaToolCallEmit {
    id: String,
    #[serde(rename = "type")]
    typ: &'static str,
    function: OaToolFn,
}

#[derive(Serialize)]
struct OaToolFn {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct OaTool<'a> {
    #[serde(rename = "type")]
    typ: &'static str,
    function: OaToolDef<'a>,
}

#[derive(Serialize)]
struct OaToolDef<'a> {
    name: String,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

#[derive(Deserialize)]
struct OaResponse {
    choices: Vec<OaChoice>,
    usage: OaUsage,
}

#[derive(Deserialize)]
struct OaChoice {
    message: OaMessageIn,
}

#[derive(Deserialize)]
struct OaMessageIn {
    content: Option<String>,
    tool_calls: Option<Vec<OaToolCallIn>>,
}

#[derive(Deserialize)]
struct OaToolCallIn {
    id: String,
    function: OaToolFnIn,
}

#[derive(Deserialize)]
struct OaToolFnIn {
    name: String,
    arguments: String, // JSON-encoded string per OpenAI spec
}

#[derive(Deserialize)]
struct OaUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

impl LlmBackend for OpenAiLlmBackend {
    fn complete<'a>(
        &'a self,
        req: LlmRequest,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, LlmError>> + Send + 'a>> {
        Box::pin(async move {
            let messages = build_messages(&req);
            let tools: Option<Vec<OaTool<'_>>> = if req.tools.is_empty() {
                None
            } else {
                Some(req.tools.iter().map(build_tool).collect())
            };
            let body = OaRequest {
                model: &req.provider.model,
                messages,
                tools,
                tool_choice: "auto",
            };
            let url = format!("{}/v1/chat/completions", self.base_url);
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| LlmError::Transport(e.to_string()))?;
            let status = resp.status();
            if status.is_server_error() {
                return Err(LlmError::ServiceUnavailable);
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(LlmError::BadRequest(format!("{status}: {text}")));
            }
            let oa: OaResponse = resp
                .json()
                .await
                .map_err(|e| LlmError::Decode(e.to_string()))?;
            let choice = oa
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| LlmError::Decode("no choices".into()))?;
            let tool_calls = choice
                .message
                .tool_calls
                .unwrap_or_default()
                .into_iter()
                .map(|c| {
                    let args: serde_json::Value = serde_json::from_str(&c.function.arguments)
                        .unwrap_or(serde_json::json!({}));
                    let (extension_id, tool_name) = split_tool_name(&c.function.name);
                    ToolCallRecord {
                        call_id: c.id,
                        extension_id,
                        tool_name,
                        args,
                    }
                })
                .collect();
            Ok(LlmResponse {
                content: choice.message.content,
                tool_calls,
                tokens_in: oa.usage.prompt_tokens,
                tokens_out: oa.usage.completion_tokens,
            })
        })
    }
}

/// Split an LLM-emitted tool name like `"http.fetch"` into
/// `(extension_id, tool_name)`. No dot → `("", whole)`.
fn split_tool_name(name: &str) -> (String, String) {
    match name.split_once('.') {
        Some((ext, tool)) => (ext.to_string(), tool.to_string()),
        None => (String::new(), name.to_string()),
    }
}

fn build_messages(req: &LlmRequest) -> Vec<OaMessage> {
    let mut out: Vec<OaMessage> = Vec::with_capacity(req.history.len() + 1);
    out.push(OaMessage::System {
        content: req.system_prompt.clone(),
    });
    for m in &req.history {
        match m {
            ChatMessage::System { content } => out.push(OaMessage::System {
                content: content.clone(),
            }),
            ChatMessage::User { content } => out.push(OaMessage::User {
                content: content.clone(),
            }),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let calls = tool_calls
                    .iter()
                    .map(|tc| OaToolCallEmit {
                        id: tc.call_id.clone(),
                        typ: "function",
                        function: OaToolFn {
                            name: format!("{}.{}", tc.extension_id, tc.tool_name),
                            arguments: tc.args.to_string(),
                        },
                    })
                    .collect();
                out.push(OaMessage::Assistant {
                    content: Some(content.clone()),
                    tool_calls: calls,
                });
            }
            ChatMessage::Tool { call_id, content } => {
                out.push(OaMessage::Tool {
                    tool_call_id: call_id.clone(),
                    content: content.to_string(),
                });
            }
        }
    }
    out
}

fn build_tool(t: &LlmToolSchema) -> OaTool<'_> {
    OaTool {
        typ: "function",
        function: OaToolDef {
            name: format!("{}.{}", t.extension_id, t.tool_name),
            description: &t.description,
            parameters: &t.parameters,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tool_name_parses_extension_prefix() {
        assert_eq!(
            split_tool_name("http.fetch"),
            ("http".into(), "fetch".into())
        );
        assert_eq!(
            split_tool_name("toolname-no-ext"),
            (String::new(), "toolname-no-ext".into())
        );
    }
}
