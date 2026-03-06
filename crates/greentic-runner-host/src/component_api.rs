use anyhow::Result;

pub mod v0_4 {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:component@0.4.0;

        interface control {
          should-cancel: func() -> bool;
          yield-now: func();
        }

        interface node {
          type json = string;

          record tenant-ctx {
            tenant: string,
            team: option<string>,
            user: option<string>,
            trace-id: option<string>,
            correlation-id: option<string>,
            deadline-unix-ms: option<u64>,
            attempt: u32,
            idempotency-key: option<string>,
          }

          record exec-ctx {
            tenant: tenant-ctx,
            flow-id: string,
            node-id: option<string>,
          }

          record node-error {
            code: string,
            message: string,
            retryable: bool,
            backoff-ms: option<u64>,
            details: option<json>,
          }

          variant invoke-result {
            ok(json),
            err(node-error),
          }

          variant stream-event {
            data(json),
            progress(u8),
            done,
            error(string),
          }

          enum lifecycle-status { ok }

          get-manifest: func() -> json;
          on-start: func(ctx: exec-ctx) -> result<lifecycle-status, string>;
          on-stop: func(ctx: exec-ctx, reason: string) -> result<lifecycle-status, string>;
          invoke: func(ctx: exec-ctx, op: string, input: json) -> invoke-result;
          invoke-stream: func(ctx: exec-ctx, op: string, input: json) -> list<stream-event>;
        }

        world component {
          import control;
          export node;
        }
        "#,
        world: "component",
    });
}

pub mod v0_5 {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:component@0.5.0;

        interface control {
          should-cancel: func() -> bool;
          yield-now: func();
        }

        interface node {
          type json = string;

          record impersonation {
            actor-id: string,
            reason: option<string>,
          }

          record tenant-ctx {
            env: string,
            tenant: string,
            tenant-id: string,
            team: option<string>,
            team-id: option<string>,
            user: option<string>,
            user-id: option<string>,
            trace-id: option<string>,
            i18n-id: option<string>,
            correlation-id: option<string>,
            attributes: list<tuple<string, string>>,
            session-id: option<string>,
            flow-id: option<string>,
            node-id: option<string>,
            provider-id: option<string>,
            deadline-ms: option<s64>,
            attempt: u32,
            idempotency-key: option<string>,
            impersonation: option<impersonation>,
          }

          record exec-ctx {
            tenant: tenant-ctx,
            i18n-id: option<string>,
            flow-id: string,
            node-id: option<string>,
          }

          record node-error {
            code: string,
            message: string,
            retryable: bool,
            backoff-ms: option<u64>,
            details: option<json>,
          }

          variant invoke-result {
            ok(json),
            err(node-error),
          }

          variant stream-event {
            data(json),
            progress(u8),
            done,
            error(string),
          }

          enum lifecycle-status { ok }

          get-manifest: func() -> json;
          on-start: func(ctx: exec-ctx) -> result<lifecycle-status, string>;
          on-stop: func(ctx: exec-ctx, reason: string) -> result<lifecycle-status, string>;
          invoke: func(ctx: exec-ctx, op: string, input: json) -> invoke-result;
          invoke-stream: func(ctx: exec-ctx, op: string, input: json) -> list<stream-event>;
        }

        world component {
          import control;
          export node;
        }
        "#,
        world: "component",
    });
}

pub mod v0_6_descriptor {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:component@0.6.0;

        interface component-descriptor {
          describe: func() -> list<u8>;
        }

        world component-v0-v6-v0 {
          export component-descriptor;
        }
        "#,
        world: "component-v0-v6-v0",
    });
}

// Component ABI bridge for runner-host: adds greentic:component/node@0.6 invoke envelope/result adapters and
// keeps backward compatibility with v0.5/v0.4.
pub mod v0_6 {
    wasmtime::component::bindgen!({
        inline: r#"
        package greentic:component@0.6.0;

        interface node {
          type capability-id = string;
          type component-id = string;
          type flow-id = string;
          type step-id = string;
          type tenant-id = string;
          type team-id = string;
          type user-id = string;
          type env-id = string;
          type trace-id = string;
          type correlation-id = string;

          record node-error {
            code: string,
            message: string,
            retryable: bool,
            backoff-ms: option<u64>,
            details: option<list<u8>>,
          }

          record tenant-ctx {
            tenant-id: tenant-id,
            team-id: option<team-id>,
            user-id: option<user-id>,
            env-id: env-id,
            trace-id: trace-id,
            correlation-id: correlation-id,
            deadline-ms: u64,
            attempt: u32,
            idempotency-key: option<string>,
            i18n-id: string,
          }

          record invocation-envelope {
            ctx: tenant-ctx,
            flow-id: flow-id,
            step-id: step-id,
            component-id: component-id,
            attempt: u32,
            payload-cbor: list<u8>,
            metadata-cbor: option<list<u8>>,
          }

          record invocation-result {
            ok: bool,
            output-cbor: list<u8>,
            output-metadata-cbor: option<list<u8>>,
          }

          invoke: func(op: string, envelope: invocation-envelope) -> result<invocation-result, node-error>;
        }

        world component {
          export node;
        }
        "#,
        world: "component",
    });
}

pub mod node {
    pub type Json = String;

    #[derive(Clone, Debug)]
    pub struct TenantCtx {
        pub tenant: String,
        pub team: Option<String>,
        pub user: Option<String>,
        pub trace_id: Option<String>,
        pub i18n_id: Option<String>,
        pub correlation_id: Option<String>,
        pub deadline_unix_ms: Option<u64>,
        pub attempt: u32,
        pub idempotency_key: Option<String>,
    }

    #[derive(Clone, Debug)]
    pub struct ExecCtx {
        pub tenant: TenantCtx,
        pub i18n_id: Option<String>,
        pub flow_id: String,
        pub node_id: Option<String>,
    }

    #[derive(Clone, Debug)]
    pub struct NodeError {
        pub code: String,
        pub message: String,
        pub retryable: bool,
        pub backoff_ms: Option<u64>,
        pub details: Option<Json>,
    }

    #[derive(Clone, Debug)]
    pub enum InvokeResult {
        Ok(Json),
        Err(NodeError),
    }
}

pub fn exec_ctx_v0_4(ctx: &node::ExecCtx) -> v0_4::exports::greentic::component::node::ExecCtx {
    v0_4::exports::greentic::component::node::ExecCtx {
        tenant: v0_4::exports::greentic::component::node::TenantCtx {
            tenant: ctx.tenant.tenant.clone(),
            team: ctx.tenant.team.clone(),
            user: ctx.tenant.user.clone(),
            trace_id: ctx.tenant.trace_id.clone(),
            correlation_id: ctx.tenant.correlation_id.clone(),
            deadline_unix_ms: ctx.tenant.deadline_unix_ms,
            attempt: ctx.tenant.attempt,
            idempotency_key: ctx.tenant.idempotency_key.clone(),
        },
        flow_id: ctx.flow_id.clone(),
        node_id: ctx.node_id.clone(),
    }
}

pub fn exec_ctx_v0_5(ctx: &node::ExecCtx) -> v0_5::exports::greentic::component::node::ExecCtx {
    let env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
    let team_id = ctx.tenant.team.clone();
    let user_id = ctx.tenant.user.clone();
    let deadline_ms = ctx
        .tenant
        .deadline_unix_ms
        .and_then(|value| i64::try_from(value).ok());
    v0_5::exports::greentic::component::node::ExecCtx {
        tenant: v0_5::exports::greentic::component::node::TenantCtx {
            env,
            tenant: ctx.tenant.tenant.clone(),
            tenant_id: ctx.tenant.tenant.clone(),
            team: ctx.tenant.team.clone(),
            team_id,
            user: ctx.tenant.user.clone(),
            user_id,
            trace_id: ctx.tenant.trace_id.clone(),
            i18n_id: ctx.tenant.i18n_id.clone(),
            correlation_id: ctx.tenant.correlation_id.clone(),
            attributes: Vec::new(),
            session_id: ctx.tenant.correlation_id.clone(),
            flow_id: Some(ctx.flow_id.clone()),
            node_id: ctx.node_id.clone(),
            provider_id: None,
            deadline_ms,
            attempt: ctx.tenant.attempt,
            idempotency_key: ctx.tenant.idempotency_key.clone(),
            impersonation: None,
        },
        i18n_id: ctx.i18n_id.clone(),
        flow_id: ctx.flow_id.clone(),
        node_id: ctx.node_id.clone(),
    }
}

pub fn envelope_v0_6(
    ctx: &node::ExecCtx,
    component_id: &str,
    payload_json: &str,
) -> Result<v0_6::exports::greentic::component::node::InvocationEnvelope> {
    let env = std::env::var("GREENTIC_ENV").unwrap_or_else(|_| "local".to_string());
    let step_id = ctx
        .node_id
        .clone()
        .unwrap_or_else(|| "component.exec".to_string());
    let i18n_id = ctx
        .i18n_id
        .clone()
        .or_else(|| ctx.tenant.i18n_id.clone())
        .unwrap_or_default();
    let trace_id = ctx.tenant.trace_id.clone().unwrap_or_default();
    let correlation_id = ctx.tenant.correlation_id.clone().unwrap_or_default();
    let deadline_ms = ctx.tenant.deadline_unix_ms.unwrap_or(u64::MAX);
    let payload_cbor = if let Ok(invocation) =
        serde_json::from_str::<greentic_types::InvocationEnvelope>(payload_json)
    {
        match serde_json::from_slice::<serde_json::Value>(&invocation.payload) {
            Ok(value) => serde_cbor::to_vec(&value)?,
            Err(_) => serde_cbor::to_vec(&serde_cbor::Value::Bytes(invocation.payload))?,
        }
    } else {
        let payload_value = serde_json::from_str::<serde_json::Value>(payload_json)
            .unwrap_or_else(|_| serde_json::Value::String(payload_json.to_string()));
        serde_cbor::to_vec(&payload_value)?
    };

    Ok(
        v0_6::exports::greentic::component::node::InvocationEnvelope {
            ctx: v0_6::exports::greentic::component::node::TenantCtx {
                tenant_id: ctx.tenant.tenant.clone(),
                team_id: ctx.tenant.team.clone(),
                user_id: ctx.tenant.user.clone(),
                env_id: env,
                trace_id,
                correlation_id,
                deadline_ms,
                attempt: ctx.tenant.attempt,
                idempotency_key: ctx.tenant.idempotency_key.clone(),
                i18n_id,
            },
            flow_id: ctx.flow_id.clone(),
            step_id,
            component_id: component_id.to_string(),
            attempt: ctx.tenant.attempt,
            payload_cbor,
            metadata_cbor: None,
        },
    )
}

pub fn invoke_result_from_v0_4(
    result: v0_4::exports::greentic::component::node::InvokeResult,
) -> node::InvokeResult {
    match result {
        v0_4::exports::greentic::component::node::InvokeResult::Ok(body) => {
            node::InvokeResult::Ok(body)
        }
        v0_4::exports::greentic::component::node::InvokeResult::Err(err) => {
            node::InvokeResult::Err(node::NodeError {
                code: err.code,
                message: err.message,
                retryable: err.retryable,
                backoff_ms: err.backoff_ms,
                details: err.details,
            })
        }
    }
}

pub fn invoke_result_from_v0_6(
    result: std::result::Result<
        v0_6::exports::greentic::component::node::InvocationResult,
        v0_6::exports::greentic::component::node::NodeError,
    >,
) -> Result<node::InvokeResult> {
    match result {
        Ok(ok) => {
            let body = cbor_to_json_string(&ok.output_cbor);
            if ok.ok {
                Ok(node::InvokeResult::Ok(body))
            } else {
                Ok(node::InvokeResult::Err(node::NodeError {
                    code: "COMPONENT_INVOCATION_FAILED".to_string(),
                    message: body,
                    retryable: false,
                    backoff_ms: None,
                    details: None,
                }))
            }
        }
        Err(err) => Ok(node::InvokeResult::Err(node::NodeError {
            code: err.code,
            message: err.message,
            retryable: err.retryable,
            backoff_ms: err.backoff_ms,
            details: err.details.map(|bytes| cbor_to_json_string(bytes.as_ref())),
        })),
    }
}

pub fn invoke_result_from_v0_5(
    result: v0_5::exports::greentic::component::node::InvokeResult,
) -> node::InvokeResult {
    match result {
        v0_5::exports::greentic::component::node::InvokeResult::Ok(body) => {
            node::InvokeResult::Ok(body)
        }
        v0_5::exports::greentic::component::node::InvokeResult::Err(err) => {
            node::InvokeResult::Err(node::NodeError {
                code: err.code,
                message: err.message,
                retryable: err.retryable,
                backoff_ms: err.backoff_ms,
                details: err.details,
            })
        }
    }
}

fn cbor_to_json_string(bytes: &[u8]) -> String {
    match serde_cbor::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|value| serde_json::to_string(&value).ok())
    {
        Some(json) => json,
        None => String::from_utf8_lossy(bytes).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{envelope_v0_6, node};
    use greentic_types::{EnvId, InvocationEnvelope, TenantCtx, TenantId};
    use std::str::FromStr;

    fn sample_exec_ctx() -> node::ExecCtx {
        node::ExecCtx {
            tenant: node::TenantCtx {
                tenant: "tenant.demo".to_string(),
                team: Some("team.demo".to_string()),
                user: Some("user.demo".to_string()),
                trace_id: Some("trace.demo".to_string()),
                i18n_id: Some("en-US".to_string()),
                correlation_id: Some("corr.demo".to_string()),
                deadline_unix_ms: Some(123),
                attempt: 2,
                idempotency_key: Some("idem.demo".to_string()),
            },
            i18n_id: Some("en-US".to_string()),
            flow_id: "flow.demo".to_string(),
            node_id: Some("node.demo".to_string()),
        }
    }

    #[test]
    fn envelope_v0_6_preserves_binary_payload_when_envelope_payload_is_not_json() {
        let envelope = InvocationEnvelope {
            ctx: TenantCtx::new(
                EnvId::from_str("local").expect("valid env id"),
                TenantId::from_str("tenant.default").expect("valid tenant id"),
            ),
            flow_id: "flow.demo".to_string(),
            node_id: Some("node.demo".to_string()),
            op: "on_message".to_string(),
            payload: vec![255, 0, 1, 42],
            metadata: Vec::new(),
        };
        let payload_json = serde_json::to_string(&envelope).expect("serialize invocation envelope");
        let envelope = envelope_v0_6(&sample_exec_ctx(), "component.demo", &payload_json)
            .expect("envelope conversion");
        let decoded: serde_cbor::Value =
            serde_cbor::from_slice(&envelope.payload_cbor).expect("decode cbor");
        assert_eq!(
            decoded,
            serde_cbor::Value::Bytes(vec![255, 0, 1, 42]),
            "binary payload must not be dropped when not valid json bytes"
        );
    }
}
