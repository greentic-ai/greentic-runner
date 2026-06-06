//! OpenAI Chat Completions API client (function-calling mode).
//! Single backend MVP; multi-provider routing deferred (spec §10).

use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::LlmError;
use crate::llm::{LlmBackend, LlmRequest, LlmResponse, LlmToolSchema, OnDelta};
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
    /// `Some(true)` on the streaming path; `None` (omitted) on the
    /// blocking path so the request body is byte-identical to before.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// `Some({"include_usage": true})` on the streaming path so the final
    /// chunk carries token usage; `None` (omitted) on the blocking path.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
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
        // OpenAI rejects `tool_calls: []` (400, code `empty_array`) on a
        // multi-turn request — omit the field entirely for a text-only
        // assistant turn (it's only valid with >=1 call).
        #[serde(skip_serializing_if = "Vec::is_empty")]
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
                stream: None,
                stream_options: None,
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

    fn complete_streaming<'a>(
        &'a self,
        req: LlmRequest,
        on_delta: OnDelta,
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
                stream: Some(true),
                stream_options: Some(serde_json::json!({ "include_usage": true })),
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
            // A non-2xx response is an error body, not an SSE stream — read
            // it whole and map exactly like the blocking path.
            if status.is_server_error() {
                return Err(LlmError::ServiceUnavailable);
            }
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(LlmError::BadRequest(format!("{status}: {text}")));
            }

            let mut acc = StreamAccumulator::new();
            let mut buf = String::new();
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.map_err(|e| LlmError::Transport(e.to_string()))?;
                buf.push_str(&String::from_utf8_lossy(&bytes));
                // Process every complete line; keep the trailing remainder.
                while let Some(idx) = buf.find('\n') {
                    let line: String = buf.drain(..=idx).collect();
                    let line = line.trim_end_matches(['\r', '\n']);
                    if let Some(text) = acc.push_line(line)? {
                        on_delta(&text);
                    }
                }
            }
            // Flush any trailing partial line without a terminating newline.
            if !buf.is_empty() {
                let line = buf.trim_end_matches(['\r', '\n']).to_string();
                if let Some(text) = acc.push_line(&line)? {
                    on_delta(&text);
                }
            }
            Ok(acc.finish())
        })
    }
}

/// Pure accumulator for OpenAI streaming (SSE) chat-completion chunks.
///
/// Fed one `data: ...` line at a time via [`StreamAccumulator::push_line`],
/// it concatenates content deltas, reassembles indexed tool-call fragments
/// (`id`/`name` arrive once; `arguments` concatenate across chunks), and
/// captures token usage from the final chunk. [`StreamAccumulator::finish`]
/// projects the accumulated state into an [`LlmResponse`], reusing the same
/// `split_tool_name` + `unwrap_or(json!({}))` argument parsing as the
/// blocking path.
struct StreamAccumulator {
    content: String,
    /// Tool-call fragments keyed by their stream `index` (stable ordering).
    tool_calls: BTreeMap<u32, ToolCallFrag>,
    tokens_in: u32,
    tokens_out: u32,
}

#[derive(Default)]
struct ToolCallFrag {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    fn new() -> Self {
        Self {
            content: String::new(),
            tool_calls: BTreeMap::new(),
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    /// Parse one streaming line. Returns `Ok(Some(text))` when the line
    /// carried a content delta to emit, `Ok(None)` for non-content lines
    /// (tool-call fragments, usage, keep-alives, `[DONE]`, blank lines).
    fn push_line(&mut self, line: &str) -> Result<Option<String>, LlmError> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let payload = match line.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None => return Ok(None), // comments / keep-alives
        };
        if payload == "[DONE]" {
            return Ok(None);
        }
        let chunk: OaStreamChunk =
            serde_json::from_str(payload).map_err(|e| LlmError::Decode(e.to_string()))?;
        if let Some(usage) = chunk.usage {
            self.tokens_in = usage.prompt_tokens;
            self.tokens_out = usage.completion_tokens;
        }
        let mut emit: Option<String> = None;
        for choice in chunk.choices {
            let delta = choice.delta;
            if let Some(text) = delta.content
                && !text.is_empty()
            {
                self.content.push_str(&text);
                emit = Some(text);
            }
            for tc in delta.tool_calls.unwrap_or_default() {
                let frag = self.tool_calls.entry(tc.index).or_default();
                if let Some(id) = tc.id {
                    frag.id = id;
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        frag.name = name;
                    }
                    if let Some(args) = func.arguments {
                        frag.arguments.push_str(&args);
                    }
                }
            }
        }
        Ok(emit)
    }

    fn finish(self) -> LlmResponse {
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|frag| {
                let args: serde_json::Value =
                    serde_json::from_str(&frag.arguments).unwrap_or(serde_json::json!({}));
                let (extension_id, tool_name) = split_tool_name(&frag.name);
                ToolCallRecord {
                    call_id: frag.id,
                    extension_id,
                    tool_name,
                    args,
                }
            })
            .collect();
        let content = if self.content.is_empty() {
            None
        } else {
            Some(self.content)
        };
        LlmResponse {
            content,
            tool_calls,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
        }
    }
}

#[derive(Deserialize)]
struct OaStreamChunk {
    #[serde(default)]
    choices: Vec<OaStreamChoice>,
    #[serde(default)]
    usage: Option<OaUsage>,
}

#[derive(Deserialize)]
struct OaStreamChoice {
    delta: OaStreamDelta,
}

#[derive(Deserialize, Default)]
struct OaStreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OaStreamToolCall>>,
}

#[derive(Deserialize)]
struct OaStreamToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<OaStreamToolFn>,
}

#[derive(Deserialize)]
struct OaStreamToolFn {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    #[test]
    fn accumulates_stream_lines_into_response() {
        let lines = [
            r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"kb.lookup","arguments":"{\"q\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}"#,
            r#"data: {"usage":{"prompt_tokens":7,"completion_tokens":9},"choices":[]}"#,
            "data: [DONE]",
        ];
        let mut acc = StreamAccumulator::new();
        let mut deltas = Vec::new();
        for l in lines {
            if let Some(text) = acc.push_line(l).expect("parse") {
                deltas.push(text);
            }
        }
        let resp = acc.finish();
        assert_eq!(deltas, vec!["Hel".to_string(), "lo".to_string()]);
        assert_eq!(resp.content.as_deref(), Some("Hello"));
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].tool_name, "lookup");
        assert_eq!(resp.tool_calls[0].args, serde_json::json!({"q":"x"}));
        assert_eq!(resp.tokens_in, 7);
        assert_eq!(resp.tokens_out, 9);
    }

    #[test]
    fn assistant_with_empty_tool_calls_omits_field() {
        // Regression: a text-only assistant turn must NOT serialise
        // `tool_calls: []` (OpenAI 400, code `empty_array`).
        use crate::config::LlmProviderRef;
        use crate::state::ChatMessage;
        let req = LlmRequest {
            system_prompt: "sys".into(),
            history: vec![ChatMessage::Assistant {
                content: "hi".into(),
                tool_calls: vec![],
            }],
            tools: vec![],
            provider: LlmProviderRef {
                provider: "openai".into(),
                model: "gpt-4o".into(),
            },
        };
        let value = serde_json::to_value(build_messages(&req)).unwrap();
        // [0] = system prompt, [1] = the assistant turn.
        assert_eq!(value[1]["role"], "assistant");
        assert!(
            value[1].get("tool_calls").is_none(),
            "empty tool_calls must be omitted, got: {}",
            value[1]
        );
    }
}
