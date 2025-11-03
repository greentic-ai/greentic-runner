//! Runtime integration with Wasmtime for invoking the MCP component entrypoint.

use std::time::Instant;

use greentic_interfaces::host_import_v0_2::{self as host_api, HostImports};
use greentic_types::TenantCtx;
use host_api::greentic::host_import::imports;
use serde_json::{json, Value};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

use crate::config::RuntimePolicy;
use crate::error::RunnerError;
use crate::verify::VerifiedArtifact;
use crate::ExecRequest;

pub struct ExecutionContext<'a> {
    pub runtime: &'a RuntimePolicy,
    pub http_enabled: bool,
    pub tenant: Option<&'a TenantCtx>,
}

pub trait Runner: Send + Sync {
    fn run(
        &self,
        request: &ExecRequest,
        artifact: &VerifiedArtifact,
        ctx: ExecutionContext<'_>,
    ) -> Result<Value, RunnerError>;
}

pub struct DefaultRunner {
    engine: Engine,
}

impl DefaultRunner {
    pub fn new(runtime: &RuntimePolicy) -> Result<Self, RunnerError> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.async_support(false);
        // Epoch interruption lets us wire wallclock enforcement without embedding async support.
        config.epoch_interruption(true);
        if runtime.fuel.is_some() {
            config.consume_fuel(true);
        }
        let engine = Engine::new(&config)?;
        Ok(Self { engine })
    }
}

impl Runner for DefaultRunner {
    fn run(
        &self,
        request: &ExecRequest,
        artifact: &VerifiedArtifact,
        ctx: ExecutionContext<'_>,
    ) -> Result<Value, RunnerError> {
        let component = match Component::from_binary(&self.engine, artifact.resolved.bytes.as_ref())
        {
            Ok(component) => component,
            Err(err) => {
                if let Some(result) =
                    try_mock_json(artifact.resolved.bytes.as_ref(), &request.action)
                {
                    return result;
                }
                return Err(err.into());
            }
        };
        let mut linker = Linker::new(&self.engine);
        linker.allow_shadowing(true);
        // Register Greentic host imports so MCP tools can call back into
        // secrets, telemetry, and HTTP helpers. Each import is handled by the
        // `HostImports` implementation on `StoreState` with conservative
        // defaults that keep the executor safe to embed.
        host_api::add_to_linker(&mut linker, |state: &mut StoreState| state)
            .map_err(RunnerError::from)?;

        let mut store = Store::new(&self.engine, StoreState::new(ctx.http_enabled, ctx.tenant));

        let instance = linker.instantiate(&mut store, &component)?;
        let exec = instance.get_typed_func::<(String, String), (String,)>(&mut store, "exec")?;

        let args_json = serde_json::to_string(&request.args)?;
        let started = Instant::now();
        let (raw_response,) = exec.call(&mut store, (request.action.clone(), args_json))?;

        if started.elapsed() > ctx.runtime.wallclock_timeout {
            return Err(RunnerError::Timeout {
                elapsed: started.elapsed(),
            });
        }

        let value: Value = serde_json::from_str(&raw_response)?;
        Ok(value)
    }
}

struct StoreState {
    http_enabled: bool,
    tenant_ctx: Option<imports::TenantCtx>,
    http_client: Option<reqwest::blocking::Client>,
}

impl StoreState {
    fn new(http_enabled: bool, tenant: Option<&TenantCtx>) -> Self {
        let tenant_ctx = tenant.map(convert_tenant_ctx);
        Self {
            http_enabled,
            tenant_ctx,
            http_client: None,
        }
    }

    fn default_tenant_ctx(&self) -> Option<imports::TenantCtx> {
        self.tenant_ctx.clone()
    }

    fn http_client(&mut self) -> Result<&reqwest::blocking::Client, imports::IfaceError> {
        if !self.http_enabled {
            return Err(imports::IfaceError::Denied);
        }

        if self.http_client.is_none() {
            // Lazily construct a blocking client so hosts that never expose
            // outbound HTTP do not pay the initialization cost.
            let client = reqwest::blocking::Client::builder()
                .use_rustls_tls()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|_| imports::IfaceError::Unavailable)?;
            self.http_client = Some(client);
        }

        Ok(self.http_client.as_ref().expect("client initialized"))
    }
}

fn convert_tenant_ctx(ctx: &TenantCtx) -> imports::TenantCtx {
    imports::TenantCtx {
        tenant: ctx.tenant.0.clone(),
        team: ctx.team.as_ref().map(|v| v.0.clone()),
        user: ctx.user.as_ref().map(|v| v.0.clone()),
        deployment: imports::DeploymentCtx {
            cloud: imports::Cloud::Other,
            region: None,
            platform: imports::Platform::Other,
            runtime: Some(ctx.env.0.clone()),
        },
        trace_id: ctx.trace_id.clone(),
    }
}

impl HostImports for StoreState {
    fn secrets_get(
        &mut self,
        key: String,
        ctx: Option<imports::TenantCtx>,
    ) -> wasmtime::Result<Result<String, imports::IfaceError>> {
        let _ = (key, ctx.or_else(|| self.default_tenant_ctx()));
        Ok(Err(imports::IfaceError::NotFound))
    }

    fn telemetry_emit(
        &mut self,
        span_json: String,
        ctx: Option<imports::TenantCtx>,
    ) -> wasmtime::Result<()> {
        if let Some(tenant) = ctx.or_else(|| self.default_tenant_ctx()) {
            tracing::debug!(
                tenant = tenant.tenant.as_str(),
                trace_id = tenant.trace_id.as_deref().unwrap_or(""),
                "telemetry::emit from component"
            );
        } else {
            tracing::debug!("telemetry::emit from component");
        }
        tracing::trace!(payload = span_json);
        Ok(())
    }

    fn tool_invoke(
        &mut self,
        tool: String,
        action: String,
        args_json: String,
        ctx: Option<imports::TenantCtx>,
    ) -> wasmtime::Result<Result<String, imports::IfaceError>> {
        let _ = (
            tool,
            action,
            args_json,
            ctx.or_else(|| self.default_tenant_ctx()),
        );
        Ok(Err(imports::IfaceError::Unavailable))
    }

    fn http_fetch(
        &mut self,
        req: imports::HttpRequest,
        ctx: Option<imports::TenantCtx>,
    ) -> wasmtime::Result<Result<imports::HttpResponse, imports::IfaceError>> {
        let _ = ctx.or_else(|| self.default_tenant_ctx());
        if !self.http_enabled {
            return Ok(Err(imports::IfaceError::Denied));
        }

        use reqwest::header::{HeaderName, HeaderValue};
        use reqwest::Method;

        let client = match self.http_client() {
            Ok(client) => client,
            Err(err) => return Ok(Err(err)),
        };

        let method = match Method::from_bytes(req.method.as_bytes()) {
            Ok(method) => method,
            Err(_) => return Ok(Err(imports::IfaceError::InvalidArg)),
        };

        let mut builder = client.request(method, &req.url);

        if let Some(headers_json) = req.headers_json.as_ref() {
            let parsed: serde_json::Map<String, Value> = match serde_json::from_str(headers_json) {
                Ok(map) => map,
                Err(_) => return Ok(Err(imports::IfaceError::InvalidArg)),
            };

            for (name, value) in parsed.iter() {
                let header_name = match HeaderName::from_bytes(name.as_bytes()) {
                    Ok(name) => name,
                    Err(_) => return Ok(Err(imports::IfaceError::InvalidArg)),
                };
                let header_value = match value {
                    Value::String(s) => HeaderValue::from_str(s),
                    Value::Array(items) => {
                        let joined = items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",");
                        HeaderValue::from_str(&joined)
                    }
                    other => HeaderValue::from_str(&other.to_string()),
                }
                .map_err(|_| imports::IfaceError::InvalidArg)?;

                builder = builder.header(header_name, header_value);
            }
        }

        if let Some(body) = req.body.clone() {
            builder = builder.body(body);
        }

        let response = match builder.send() {
            Ok(resp) => resp,
            Err(_) => return Ok(Err(imports::IfaceError::Unavailable)),
        };

        let response = match response.error_for_status() {
            Ok(resp) => resp,
            Err(_) => return Ok(Err(imports::IfaceError::Unavailable)),
        };

        let status = response.status().as_u16();
        let mut header_map = serde_json::Map::new();
        for (key, value) in response.headers().iter() {
            let entry = header_map
                .entry(key.as_str().to_string())
                .or_insert_with(|| json!([]));
            if let Value::Array(array) = entry {
                array.push(Value::String(
                    value.to_str().unwrap_or_default().to_string(),
                ));
            }
        }
        let headers_json = if header_map.is_empty() {
            None
        } else {
            serde_json::to_string(&header_map).ok()
        };

        let body_text = match response.text() {
            Ok(text) => text,
            Err(_) => return Ok(Err(imports::IfaceError::Unavailable)),
        };
        let body = if body_text.is_empty() {
            None
        } else {
            Some(body_text)
        };

        Ok(Ok(imports::HttpResponse {
            status,
            headers_json,
            body,
        }))
    }
}

fn try_mock_json(bytes: &[u8], action: &str) -> Option<Result<Value, RunnerError>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let root: Value = serde_json::from_str(text).ok()?;

    if !root
        .get("_mock_mcp_exec")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    let responses = root.get("responses")?.as_object()?;
    match responses.get(action) {
        Some(value) => Some(Ok(value.clone())),
        None => Some(Err(RunnerError::ActionNotFound {
            action: action.to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{EnvId, TenantCtx, TenantId};

    fn sample_tenant() -> TenantCtx {
        TenantCtx {
            env: EnvId("dev".into()),
            tenant: TenantId("tenant".into()),
            tenant_id: TenantId("tenant".into()),
            team: None,
            team_id: None,
            user: None,
            user_id: None,
            trace_id: Some("trace-123".into()),
            correlation_id: None,
            deadline: None,
            attempt: 0,
            idempotency_key: None,
            impersonation: None,
        }
    }

    #[test]
    fn secrets_get_defaults_to_not_found() {
        let tenant = sample_tenant();
        let mut state = StoreState::new(true, Some(&tenant));

        let result = state
            .secrets_get("api-key".into(), None)
            .expect("host call should succeed");
        assert!(matches!(result, Err(imports::IfaceError::NotFound)));
    }

    #[test]
    fn http_fetch_requires_http_flag() {
        let tenant = sample_tenant();
        let mut state = StoreState::new(false, Some(&tenant));

        let request = imports::HttpRequest {
            method: "GET".into(),
            url: "https://example.com".into(),
            headers_json: None,
            body: None,
        };

        let result = state
            .http_fetch(request, None)
            .expect("host call should succeed");
        assert!(matches!(result, Err(imports::IfaceError::Denied)));
    }

    #[test]
    fn telemetry_emit_accepts_payload() {
        let tenant = sample_tenant();
        let mut state = StoreState::new(true, Some(&tenant));

        state
            .telemetry_emit("{\"event\":\"test\"}".into(), None)
            .expect("telemetry should succeed");
    }
}
