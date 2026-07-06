//! Best-effort, bounded-channel NATS publisher for per-node audit events.
//!
//! `AuditSink::emit` must NEVER block flow execution, NEVER return an error,
//! and NEVER panic: it serializes the envelope and hands the bytes off to a
//! bounded `tokio::sync::mpsc` channel via `try_send`. If the channel is full
//! (a slow/unavailable NATS server) or closed (drain task gone), the event is
//! dropped and a `tracing::warn!` is emitted — mirroring the trace recorder's
//! existing best-effort flush semantics. The background drain task performs
//! the actual `async_nats::Client::publish` fire-and-forget (no per-message
//! `flush`), so a NATS outage never backs up onto the hot path.

use tokio::sync::mpsc::{self, Sender};

/// Bounded channel capacity for outbound audit events. Sized generously
/// relative to expected per-flow node-event volume; once full, `emit` drops
/// events rather than block flow execution.
const CHANNEL_CAPACITY: usize = 1024;

/// Best-effort audit event publisher.
///
/// Cloning is cheap (`Sender` is an `Arc`-backed handle), so a single
/// `AuditSink` can be shared across tenants/flows.
#[derive(Clone)]
pub struct AuditSink {
    tx: Sender<(String, Vec<u8>)>,
}

impl AuditSink {
    /// Builds a sink backed by `client`, spawning the background drain task
    /// on the current tokio runtime. Must be called from within a tokio
    /// runtime context.
    pub fn new(client: async_nats::Client) -> AuditSink {
        let (tx, mut rx) = mpsc::channel::<(String, Vec<u8>)>(CHANNEL_CAPACITY);

        tokio::spawn(async move {
            while let Some((subject, bytes)) = rx.recv().await {
                if let Err(e) = client.publish(subject, bytes.into()).await {
                    tracing::warn!(%e, "audit publish failed");
                }
            }
        });

        AuditSink { tx }
    }

    /// Test-only constructor that builds a sink directly from a sender,
    /// bypassing the need for a live NATS client. Lets tests observe/starve
    /// the channel directly (e.g. saturation behavior).
    #[cfg(test)]
    pub fn from_sender(tx: Sender<(String, Vec<u8>)>) -> AuditSink {
        AuditSink { tx }
    }

    /// Serializes `envelope` and enqueues it for publishing on `subject`.
    /// Best-effort: never blocks, never returns an error, never panics. If
    /// the bounded channel is full or the drain task has gone away, the
    /// event is dropped and a warning is logged.
    pub fn emit(&self, subject: String, envelope: &greentic_types::EventEnvelope) {
        let bytes = match serde_json::to_vec(envelope) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(%e, "audit event serialization failed");
                return;
            }
        };

        if let Err(e) = self.tx.try_send((subject, bytes)) {
            tracing::warn!(%e, "audit event dropped (channel full or closed)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{EnvId, EventId, TenantCtx, TenantId};
    use serde_json::Value;

    fn tenant_ctx() -> TenantCtx {
        TenantCtx::new(
            EnvId::try_from("prod").expect("valid env id"),
            TenantId::try_from("t1").expect("valid tenant id"),
        )
    }

    fn sample_envelope() -> greentic_types::EventEnvelope {
        greentic_types::EventEnvelope {
            id: EventId::new("id1").expect("valid event id"),
            topic: "runner.flow".to_string(),
            r#type: "greentic.runner.flow.node_end".to_string(),
            source: "runner".to_string(),
            tenant: tenant_ctx(),
            subject: Some("flow:f1/node:n1".to_string()),
            time: chrono::Utc::now(),
            correlation_id: Some("s1".to_string()),
            payload: serde_json::json!({"node_id": "n1"}),
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn emit_drops_without_blocking_when_channel_is_saturated() {
        // Build a channel and fill it to capacity WITHOUT a drain task
        // consuming it, so the 1025th `emit` must hit the full-channel
        // drop path rather than blocking forever.
        let (tx, mut _rx) = mpsc::channel::<(String, Vec<u8>)>(CHANNEL_CAPACITY);
        let sink = AuditSink::from_sender(tx);
        let envelope = sample_envelope();

        for _ in 0..CHANNEL_CAPACITY {
            sink.emit("audit.t1.flow.node_end".to_string(), &envelope);
        }

        // The channel is now full. This call must return immediately
        // (dropping the event) rather than block or panic.
        sink.emit("audit.t1.flow.node_end".to_string(), &envelope);

        // Keep the receiver alive until after the saturating emit so the
        // channel isn't considered closed instead of full.
        drop(_rx);
    }

    #[tokio::test]
    async fn emit_serializes_envelope_with_type_field() {
        let (tx, mut rx) = mpsc::channel::<(String, Vec<u8>)>(CHANNEL_CAPACITY);
        let sink = AuditSink::from_sender(tx);
        let envelope = sample_envelope();

        sink.emit("audit.t1.flow.node_end".to_string(), &envelope);

        let (subject, bytes) = rx.recv().await.expect("event enqueued");
        assert_eq!(subject, "audit.t1.flow.node_end");

        let value: Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(
            value.get("type").and_then(Value::as_str),
            Some("greentic.runner.flow.node_end")
        );
    }
}
