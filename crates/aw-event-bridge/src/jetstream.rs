//! JetStream durability for the agentic dispatch consumer: a `greentic-agentic`
//! stream binds `greentic.agentic.request.v1`; a durable pull consumer
//! `agentic-workers` (explicit ack) lets aw-serve replicas share the queue and
//! lets KEDA scale on consumer lag.

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer, stream};
use greentic_types::request_topic;

use crate::RUNTIME_NAME;

pub const STREAM_NAME: &str = "greentic-agentic";
pub const DURABLE_CONSUMER: &str = "agentic-workers";

/// Returns the JetStream stream config that binds `greentic.agentic.request.v1`
/// with work-queue retention (each message is removed once any consumer acks it).
#[must_use]
pub fn agentic_stream_config() -> stream::Config {
    stream::Config {
        name: STREAM_NAME.to_string(),
        subjects: vec![request_topic(RUNTIME_NAME)],
        retention: stream::RetentionPolicy::WorkQueue,
        ..Default::default()
    }
}

/// Returns the durable pull consumer config (`agentic-workers`) with explicit
/// ack and a max-deliver cap of 5 to prevent poison-pill messages from looping
/// indefinitely.
#[must_use]
pub fn agentic_consumer_config() -> consumer::pull::Config {
    consumer::pull::Config {
        durable_name: Some(DURABLE_CONSUMER.to_string()),
        ack_policy: consumer::AckPolicy::Explicit,
        max_deliver: 5,
        ..Default::default()
    }
}

/// Ensure the `greentic-agentic` stream and `agentic-workers` durable pull
/// consumer both exist, then return a handle to the consumer.
///
/// Idempotent: uses `get_or_create_stream` / `get_or_create_consumer`, so
/// calling this on every process start is safe.
#[allow(dead_code)] // used by the next task (aw-serve JetStream integration)
pub async fn ensure_consumer(
    client: &async_nats::Client,
) -> Result<consumer::Consumer<consumer::pull::Config>> {
    let js = jetstream::new(client.clone());
    let stream = js
        .get_or_create_stream(agentic_stream_config())
        .await
        .context("get_or_create greentic-agentic stream")?;
    let consumer = stream
        .get_or_create_consumer(DURABLE_CONSUMER, agentic_consumer_config())
        .await
        .context("get_or_create agentic-workers consumer")?;
    Ok(consumer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_config_binds_agentic_request_subject() {
        let cfg = agentic_stream_config();
        assert_eq!(cfg.name, "greentic-agentic");
        assert!(
            cfg.subjects.iter().any(|s| s == "greentic.agentic.request.v1"),
            "expected greentic.agentic.request.v1 in subjects, got {:?}",
            cfg.subjects
        );
    }

    #[test]
    fn stream_config_uses_work_queue_retention() {
        let cfg = agentic_stream_config();
        assert!(
            matches!(cfg.retention, stream::RetentionPolicy::WorkQueue),
            "expected WorkQueue retention, got {:?}",
            cfg.retention
        );
    }

    #[test]
    fn consumer_config_is_durable_explicit_ack() {
        let cfg = agentic_consumer_config();
        assert_eq!(
            cfg.durable_name.as_deref(),
            Some("agentic-workers"),
            "durable_name must be agentic-workers"
        );
        assert!(
            matches!(cfg.ack_policy, consumer::AckPolicy::Explicit),
            "ack_policy must be Explicit, got {:?}",
            cfg.ack_policy
        );
    }

    #[test]
    fn consumer_config_caps_max_deliver() {
        let cfg = agentic_consumer_config();
        assert_eq!(cfg.max_deliver, 5, "max_deliver should be 5 to prevent poison-pill loops");
    }
}
