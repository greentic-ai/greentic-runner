use anyhow::{Context, Result};
use greentic_types::{EnvId, InvocationEnvelope, TenantCtx, TenantId};
use serde_json::Value;
use std::str::FromStr;

/// Runtime-owned invocation metadata builder.
///
/// Runtime owns `ctx` and flow/node metadata, so flows must not embed their own context fields.
#[derive(Clone, Copy)]
pub struct InvocationMeta<'a> {
    pub env: &'a str,
    pub tenant: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub provider_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub attempt: u32,
}

pub fn build_invocation_envelope(
    meta: InvocationMeta<'_>,
    operation: &str,
    payload: Value,
) -> Result<InvocationEnvelope> {
    let parsed = InvocationPayload::parse(payload);
    let env_id = EnvId::from_str(meta.env)
        .unwrap_or_else(|_| EnvId::from_str("local").expect("local env id is valid"));
    let tenant_id = TenantId::from_str(meta.tenant).unwrap_or_else(|_| {
        tracing::warn!(
            tenant = meta.tenant,
            "invalid tenant id in invocation envelope, falling back to tenant.default"
        );
        TenantId::from_str("tenant.default").expect("tenant fallback must be valid")
    });
    let mut ctx = TenantCtx::new(env_id, tenant_id).with_flow(meta.flow_id.to_string());
    if let Some(provider) = meta.provider_id {
        ctx = ctx.with_provider(provider.to_string());
    }
    if let Some(session) = meta.session_id {
        ctx = ctx.with_session(session.to_string());
    }
    if let Some(node) = meta.node_id {
        ctx = ctx.with_node(node.to_string());
    }
    ctx = ctx.with_attempt(meta.attempt);

    let payload_bytes =
        to_binary_payload(&parsed.payload).context("serialize payload for invocation envelope")?;
    let metadata_bytes = to_binary_payload(&parsed.metadata)
        .context("serialize metadata for invocation envelope")?;

    let op = parsed.op.unwrap_or_else(|| operation.to_string());
    tracing::trace!(
        flow_id = %meta.flow_id,
        node_id = ?meta.node_id,
        op = %op,
        payload = %parsed.payload,
        "built invocation envelope; runtime owns ctx; flows must not embed ctx"
    );

    Ok(InvocationEnvelope {
        ctx,
        flow_id: meta.flow_id.to_string(),
        node_id: meta.node_id.map(str::to_string),
        op,
        payload: payload_bytes,
        metadata: metadata_bytes,
    })
}

fn to_binary_payload(value: &Value) -> Result<Vec<u8>> {
    serde_json::to_vec(value).context("failed to encode payload for envelope")
}

struct InvocationPayload {
    op: Option<String>,
    payload: Value,
    metadata: Value,
}

impl InvocationPayload {
    fn parse(value: Value) -> Self {
        if let Value::Object(mut map) = value {
            if let Some(envelope) = map.remove("envelope") {
                return Self::from_envelope(envelope);
            }
            // Accept both canonical keys (`op`, `payload`, `metadata`) and
            // compatibility aliases used by older flow payloads (`operation`,
            // `input`, `config`) without dropping sibling fields such as `tool`.
            let op = extract_string(map.remove("op").or_else(|| map.remove("operation")));
            let payload = map
                .remove("payload")
                .or_else(|| map.remove("input"))
                .unwrap_or(Value::Null);
            let metadata = map
                .remove("metadata")
                .or_else(|| map.remove("config"))
                .unwrap_or(Value::Null);
            if op.is_some() || !payload.is_null() || !metadata.is_null() {
                if payload.is_null() && !map.is_empty() {
                    return Self {
                        op,
                        payload: Value::Object(map),
                        metadata,
                    };
                }
                return Self {
                    op,
                    payload,
                    metadata,
                };
            }
            return Self {
                op: None,
                payload: Value::Object(map),
                metadata: Value::Null,
            };
        }
        InvocationPayload {
            op: None,
            payload: value,
            metadata: Value::Null,
        }
    }

    fn from_envelope(value: Value) -> Self {
        if let Value::Object(map) = value {
            let op = extract_string(map.get("op").cloned());
            let payload = map.get("payload").cloned().unwrap_or(Value::Null);
            let metadata = map.get("metadata").cloned().unwrap_or(Value::Null);
            return Self {
                op,
                payload,
                metadata,
            };
        }
        InvocationPayload {
            op: None,
            payload: value,
            metadata: Value::Null,
        }
    }
}

fn extract_string(value: Option<Value>) -> Option<String> {
    value.and_then(|v| v.as_str().map(|s| s.to_string()))
}
