//! Streaming building blocks for `POST /agent/chat/stream`: a session-keyed
//! observer registry, an SSE-forwarding `StepObserver`, and a fan-out composite
//! that lets the streaming observer coexist with the audit observer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use greentic_aw_runtime::StepObserver;
use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

/// Session-id → active streaming observer. Shared by `ServerState` (writer:
/// the SSE handler) and `RuntimeAgentNodeHandler` (reader: the agent step).
pub type StreamObserverRegistry = Arc<DashMap<String, Arc<dyn StepObserver>>>;

/// One SSE frame emitted to the designer. Mirrors the designer's `TestChatEvent`
/// wire taxonomy (`event: frame`, kebab-case `kind`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StreamFrame {
    TextChunk {
        text: String,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        status: FrameStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Done,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameStatus {
    Ok,
    Error,
}

/// A `StepObserver` that forwards token/tool callbacks as `StreamFrame`s onto an
/// unbounded `tokio::sync::mpsc` channel. The send side (this observer, called
/// from the LLM read loop) never blocks or drops a frame for backpressure
/// reasons — the channel is unbounded. The receive side is drained directly by
/// the SSE responder via `futures::stream::unfold`, which feeds `Sse::new`
/// (see `agent_chat_stream_core` / `agent_chat_stream`); there is no separate
/// bounded intermediate stage between the observer and the SSE response.
pub struct SseForwardObserver {
    tx: UnboundedSender<StreamFrame>,
    /// Flips to `true` on the first `on_token_delta` call. Lets the SSE
    /// handler's no-delta fallback tell whether the backend actually streamed
    /// token chunks this turn, so it only synthesizes a single `TextChunk`
    /// from the assembled reply for non-streaming backends (streaming
    /// backends already delivered their text via this observer).
    streamed: AtomicBool,
}

impl SseForwardObserver {
    pub fn new(tx: UnboundedSender<StreamFrame>) -> Self {
        Self {
            tx,
            streamed: AtomicBool::new(false),
        }
    }

    /// Whether this observer has forwarded at least one token delta so far.
    pub fn streamed(&self) -> bool {
        self.streamed.load(Ordering::Relaxed)
    }
}

impl StepObserver for SseForwardObserver {
    fn wants_streaming(&self) -> bool {
        true
    }
    fn on_token_delta(&self, chunk: &str) {
        self.streamed.store(true, Ordering::Relaxed);
        let _ = self.tx.send(StreamFrame::TextChunk {
            text: chunk.to_string(),
        });
    }
    fn on_tool_call(&self, name: &str, call_id: &str) {
        let _ = self.tx.send(StreamFrame::ToolCall {
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
            args: serde_json::Value::Null,
        });
    }
    fn on_tool_result(&self, _name: &str, call_id: &str, result: &serde_json::Value) {
        let _ = self.tx.send(StreamFrame::ToolResult {
            call_id: call_id.to_string(),
            status: FrameStatus::Ok,
            result: Some(result.clone()),
            error: None,
        });
    }
}

/// Fan-out `StepObserver`: forwards every callback to all members and reports
/// `wants_streaming` as the OR of its members. Used to run the SSE observer and
/// the audit observer from the same agent step.
pub struct CompositeObserver {
    members: Vec<Arc<dyn StepObserver>>,
}

impl CompositeObserver {
    pub fn new(members: Vec<Arc<dyn StepObserver>>) -> Self {
        Self { members }
    }
}

impl StepObserver for CompositeObserver {
    fn wants_streaming(&self) -> bool {
        self.members.iter().any(|m| m.wants_streaming())
    }
    fn on_token_delta(&self, chunk: &str) {
        for m in &self.members {
            m.on_token_delta(chunk);
        }
    }
    fn on_tool_call(&self, name: &str, call_id: &str) {
        for m in &self.members {
            m.on_tool_call(name, call_id);
        }
    }
    fn on_tool_result(&self, name: &str, call_id: &str, result: &serde_json::Value) {
        for m in &self.members {
            m.on_tool_result(name, call_id, result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    // Records every callback so we can assert fan-out.
    #[derive(Default)]
    struct Recorder {
        streaming: bool,
        calls: Mutex<Vec<String>>,
    }
    impl StepObserver for Recorder {
        fn wants_streaming(&self) -> bool {
            self.streaming
        }
        fn on_token_delta(&self, c: &str) {
            self.calls.lock().unwrap().push(format!("t:{c}"));
        }
        fn on_tool_call(&self, n: &str, id: &str) {
            self.calls.lock().unwrap().push(format!("c:{n}:{id}"));
        }
        fn on_tool_result(&self, n: &str, id: &str, _r: &serde_json::Value) {
            self.calls.lock().unwrap().push(format!("r:{n}:{id}"));
        }
    }

    #[test]
    fn composite_fans_out_to_all_members_and_ors_streaming() {
        let a = Arc::new(Recorder {
            streaming: false,
            ..Default::default()
        });
        let b = Arc::new(Recorder {
            streaming: true,
            ..Default::default()
        });
        let comp = CompositeObserver::new(vec![a.clone(), b.clone()]);
        assert!(comp.wants_streaming(), "OR of members");
        comp.on_token_delta("hi");
        comp.on_tool_call("email", "call_1");
        assert_eq!(*a.calls.lock().unwrap(), vec!["t:hi", "c:email:call_1"]);
        assert_eq!(*b.calls.lock().unwrap(), vec!["t:hi", "c:email:call_1"]);
    }

    #[test]
    fn sse_forward_observer_streamed_flips_true_on_first_delta() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let obs = SseForwardObserver::new(tx);
        assert!(!obs.streamed(), "should start unstreamed");
        obs.on_token_delta("hi");
        assert!(obs.streamed(), "should flip true after first delta");
    }

    #[test]
    fn sse_forward_observer_emits_frames_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let obs = SseForwardObserver::new(tx);
        assert!(obs.wants_streaming());
        obs.on_token_delta("Hel");
        obs.on_tool_call("sql", "c1");
        obs.on_tool_result("sql", "c1", &json!({"rows": 2}));
        obs.on_token_delta("lo");
        drop(obs);
        let mut kinds = vec![];
        while let Ok(f) = rx.try_recv() {
            kinds.push(f);
        }
        assert!(matches!(kinds[0], StreamFrame::TextChunk { ref text } if text == "Hel"));
        assert!(matches!(kinds[1], StreamFrame::ToolCall { ref call_id, .. } if call_id == "c1"));
        assert!(matches!(kinds[2], StreamFrame::ToolResult { ref call_id, .. } if call_id == "c1"));
        assert!(matches!(kinds[3], StreamFrame::TextChunk { ref text } if text == "lo"));
    }

    #[test]
    fn stream_frame_serializes_kebab_case_kind() {
        let f = StreamFrame::TextChunk { text: "hi".into() };
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            serde_json::json!({"kind":"text-chunk","text":"hi"})
        );
        let e = StreamFrame::ToolResult {
            call_id: "c1".into(),
            status: FrameStatus::Error,
            result: None,
            error: Some("boom".into()),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({"kind":"tool-result","call_id":"c1","status":"error","error":"boom"})
        );
    }
}
