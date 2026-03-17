//! Trait implementations for HostState.

use super::helpers::canonicalize_wasm_secret_key;
use super::host_state::HostState;
use crate::provider_core_only;
use crate::secrets::{read_secret_blocking, write_secret_blocking};
use crate::storage::state::STATE_PREFIX;
use greentic_interfaces_wasmtime::host_helpers::v1::{
    http_client::{
        HttpClientError, HttpClientErrorV1_1, HttpClientHost, HttpClientHostV1_1,
        Request as HttpRequest, RequestOptionsV1_1 as HttpRequestOptionsV1_1,
        RequestV1_1 as HttpRequestV1_1, Response as HttpResponse, ResponseV1_1 as HttpResponseV1_1,
        TenantCtx as HttpTenantCtx, TenantCtxV1_1 as HttpTenantCtxV1_1,
    },
    runner_host_http::RunnerHostHttp,
    runner_host_kv::RunnerHostKv,
    secrets_store::{SecretsError, SecretsErrorV1_1, SecretsStoreHost, SecretsStoreHostV1_1},
    state_store::{
        OpAck as StateOpAck, StateKey as HostStateKey, StateStoreError as StateError,
        StateStoreHost, TenantCtx as StateTenantCtx,
    },
    telemetry_logger::{
        OpAck as TelemetryAck, SpanContext as TelemetrySpanContext,
        TelemetryLoggerError as TelemetryError, TelemetryLoggerHost,
        TenantCtx as TelemetryTenantCtx,
    },
};
use greentic_interfaces_wasmtime::http_client_client_v1_0::greentic::interfaces_types::types as http_types_v1_0;
use greentic_types::StateKey as StoreStateKey;
use serde_json::Value;
use tracing::warn;

#[cfg(feature = "fault-injection")]
use crate::testing::fault_injection::{FaultContext, FaultPoint, maybe_fail};

impl SecretsStoreHost for HostState {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, SecretsError> {
        if provider_core_only::is_enabled() {
            warn!(secret = %key, "provider-core only mode enabled; blocking secrets store");
            return Err(SecretsError::Denied);
        }
        if !self.config.secrets_policy.is_allowed(&key) {
            return Err(SecretsError::Denied);
        }
        if let Some(mock) = &self.mocks
            && let Some(value) = mock.secrets_lookup(&key)
        {
            return Ok(Some(value.into_bytes()));
        }
        let ctx = self.secrets_tenant_ctx();
        let canonical_key = canonicalize_wasm_secret_key(&key);
        match read_secret_blocking(&self.secrets, &ctx, &self.pack_id, &canonical_key) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) => {
                warn!(secret = %key, canonical = %canonical_key, error = %err, "secret lookup failed");
                Err(SecretsError::NotFound)
            }
        }
    }
}

impl SecretsStoreHostV1_1 for HostState {
    fn get(&mut self, key: String) -> Result<Option<Vec<u8>>, SecretsErrorV1_1> {
        if provider_core_only::is_enabled() {
            warn!(secret = %key, "provider-core only mode enabled; blocking secrets store");
            return Err(SecretsErrorV1_1::Denied);
        }
        if !self.config.secrets_policy.is_allowed(&key) {
            return Err(SecretsErrorV1_1::Denied);
        }
        if let Some(mock) = &self.mocks
            && let Some(value) = mock.secrets_lookup(&key)
        {
            return Ok(Some(value.into_bytes()));
        }
        let ctx = self.secrets_tenant_ctx();
        let canonical_key = canonicalize_wasm_secret_key(&key);
        match read_secret_blocking(&self.secrets, &ctx, &self.pack_id, &canonical_key) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) => {
                warn!(secret = %key, canonical = %canonical_key, error = %err, "secret lookup failed");
                Err(SecretsErrorV1_1::NotFound)
            }
        }
    }

    fn put(&mut self, key: String, value: Vec<u8>) {
        if key.trim().is_empty() {
            warn!(secret = %key, "secret write blocked: empty key");
            panic!("secret write denied for key {key}: invalid key");
        }
        if provider_core_only::is_enabled() && !self.allows_secret_write_in_provider_core_only() {
            warn!(
                secret = %key,
                component = self.component_ref.as_deref().unwrap_or("<pack>"),
                "provider-core only mode enabled; blocking secrets store write"
            );
            panic!("secret write denied for key {key}: provider-core-only mode");
        }
        if !self.config.secrets_policy.is_allowed(&key) {
            warn!(secret = %key, "secret write denied by bindings policy");
            panic!("secret write denied for key {key}: policy");
        }
        let ctx = self.secrets_tenant_ctx();
        let canonical_key = canonicalize_wasm_secret_key(&key);
        if let Err(err) =
            write_secret_blocking(&self.secrets, &ctx, &self.pack_id, &canonical_key, &value)
        {
            warn!(secret = %key, canonical = %canonical_key, error = %err, "secret write failed");
            panic!("secret write failed for key {key}");
        }
    }
}

impl HttpClientHost for HostState {
    fn send(
        &mut self,
        req: HttpRequest,
        ctx: Option<HttpTenantCtx>,
    ) -> Result<HttpResponse, HttpClientError> {
        self.send_http_request(req, None, ctx)
    }
}

impl HttpClientHostV1_1 for HostState {
    fn send(
        &mut self,
        req: HttpRequestV1_1,
        opts: Option<HttpRequestOptionsV1_1>,
        ctx: Option<HttpTenantCtxV1_1>,
    ) -> Result<HttpResponseV1_1, HttpClientErrorV1_1> {
        let legacy_req = HttpRequest {
            method: req.method,
            url: req.url,
            headers: req.headers,
            body: req.body,
        };
        let legacy_ctx = ctx.map(|ctx| HttpTenantCtx {
            env: ctx.env,
            tenant: ctx.tenant,
            tenant_id: ctx.tenant_id,
            team: ctx.team,
            team_id: ctx.team_id,
            user: ctx.user,
            user_id: ctx.user_id,
            trace_id: ctx.trace_id,
            correlation_id: ctx.correlation_id,
            i18n_id: ctx.i18n_id,
            attributes: ctx.attributes,
            session_id: ctx.session_id,
            flow_id: ctx.flow_id,
            node_id: ctx.node_id,
            provider_id: ctx.provider_id,
            deadline_ms: ctx.deadline_ms,
            attempt: ctx.attempt,
            idempotency_key: ctx.idempotency_key,
            impersonation: ctx.impersonation.map(|imp| http_types_v1_0::Impersonation {
                actor_id: imp.actor_id,
                reason: imp.reason,
            }),
        });

        self.send_http_request(legacy_req, opts, legacy_ctx)
            .map(|resp| HttpResponseV1_1 {
                status: resp.status,
                headers: resp.headers,
                body: resp.body,
            })
            .map_err(|err| HttpClientErrorV1_1 {
                code: err.code,
                message: err.message,
            })
    }
}

impl StateStoreHost for HostState {
    fn read(
        &mut self,
        key: HostStateKey,
        ctx: Option<StateTenantCtx>,
    ) -> Result<Vec<u8>, StateError> {
        let store = match self.state_store.as_ref() {
            Some(store) => store.clone(),
            None => {
                return Err(StateError {
                    code: "unavailable".into(),
                    message: "state store not configured".into(),
                });
            }
        };
        let tenant_ctx = match self.tenant_ctx_from_v1(ctx) {
            Ok(ctx) => ctx,
            Err(err) => {
                return Err(StateError {
                    code: "invalid-ctx".into(),
                    message: err.to_string(),
                });
            }
        };
        #[cfg(feature = "fault-injection")]
        {
            let exec_ctx = self.exec_ctx.as_ref();
            let flow_id = exec_ctx
                .map(|ctx| ctx.flow_id.as_str())
                .unwrap_or("unknown");
            let node_id = exec_ctx.and_then(|ctx| ctx.node_id.as_deref());
            let attempt = exec_ctx.map(|ctx| ctx.tenant.attempt).unwrap_or(1);
            let fault_ctx = FaultContext {
                pack_id: self.pack_id.as_str(),
                flow_id,
                node_id,
                attempt,
            };
            if let Err(err) = maybe_fail(FaultPoint::StateRead, fault_ctx) {
                return Err(StateError {
                    code: "internal".into(),
                    message: err.to_string(),
                });
            }
        }
        let key = StoreStateKey::from(key);
        match store.get_json(&tenant_ctx, STATE_PREFIX, &key, None) {
            Ok(Some(value)) => Ok(serde_json::to_vec(&value).unwrap_or_else(|_| Vec::new())),
            Ok(None) => Err(StateError {
                code: "not_found".into(),
                message: "state key not found".into(),
            }),
            Err(err) => Err(StateError {
                code: "internal".into(),
                message: err.to_string(),
            }),
        }
    }

    fn write(
        &mut self,
        key: HostStateKey,
        bytes: Vec<u8>,
        ctx: Option<StateTenantCtx>,
    ) -> Result<StateOpAck, StateError> {
        let store = match self.state_store.as_ref() {
            Some(store) => store.clone(),
            None => {
                return Err(StateError {
                    code: "unavailable".into(),
                    message: "state store not configured".into(),
                });
            }
        };
        let tenant_ctx = match self.tenant_ctx_from_v1(ctx) {
            Ok(ctx) => ctx,
            Err(err) => {
                return Err(StateError {
                    code: "invalid-ctx".into(),
                    message: err.to_string(),
                });
            }
        };
        #[cfg(feature = "fault-injection")]
        {
            let exec_ctx = self.exec_ctx.as_ref();
            let flow_id = exec_ctx
                .map(|ctx| ctx.flow_id.as_str())
                .unwrap_or("unknown");
            let node_id = exec_ctx.and_then(|ctx| ctx.node_id.as_deref());
            let attempt = exec_ctx.map(|ctx| ctx.tenant.attempt).unwrap_or(1);
            let fault_ctx = FaultContext {
                pack_id: self.pack_id.as_str(),
                flow_id,
                node_id,
                attempt,
            };
            if let Err(err) = maybe_fail(FaultPoint::StateWrite, fault_ctx) {
                return Err(StateError {
                    code: "internal".into(),
                    message: err.to_string(),
                });
            }
        }
        let key = StoreStateKey::from(key);
        let value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
        match store.set_json(&tenant_ctx, STATE_PREFIX, &key, None, &value, None) {
            Ok(()) => Ok(StateOpAck::Ok),
            Err(err) => Err(StateError {
                code: "internal".into(),
                message: err.to_string(),
            }),
        }
    }

    fn delete(
        &mut self,
        key: HostStateKey,
        ctx: Option<StateTenantCtx>,
    ) -> Result<StateOpAck, StateError> {
        let store = match self.state_store.as_ref() {
            Some(store) => store.clone(),
            None => {
                return Err(StateError {
                    code: "unavailable".into(),
                    message: "state store not configured".into(),
                });
            }
        };
        let tenant_ctx = match self.tenant_ctx_from_v1(ctx) {
            Ok(ctx) => ctx,
            Err(err) => {
                return Err(StateError {
                    code: "invalid-ctx".into(),
                    message: err.to_string(),
                });
            }
        };
        let key = StoreStateKey::from(key);
        match store.del(&tenant_ctx, STATE_PREFIX, &key) {
            Ok(_) => Ok(StateOpAck::Ok),
            Err(err) => Err(StateError {
                code: "internal".into(),
                message: err.to_string(),
            }),
        }
    }
}

impl TelemetryLoggerHost for HostState {
    fn log(
        &mut self,
        span: TelemetrySpanContext,
        fields: Vec<(String, String)>,
        _ctx: Option<TelemetryTenantCtx>,
    ) -> Result<TelemetryAck, TelemetryError> {
        if let Some(mock) = &self.mocks
            && mock.telemetry_drain(&[("span_json", span.flow_id.as_str())])
        {
            return Ok(TelemetryAck::Ok);
        }
        let mut map = serde_json::Map::new();
        for (k, v) in fields {
            map.insert(k, Value::String(v));
        }
        tracing::info!(
            tenant = %span.tenant,
            flow_id = %span.flow_id,
            node = ?span.node_id,
            provider = %span.provider,
            fields = %serde_json::Value::Object(map.clone()),
            "telemetry log from pack"
        );
        Ok(TelemetryAck::Ok)
    }
}

impl RunnerHostHttp for HostState {
    fn request(
        &mut self,
        method: String,
        url: String,
        headers: Vec<String>,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, String> {
        let req = HttpRequest {
            method,
            url,
            headers: headers
                .chunks(2)
                .filter_map(|chunk| {
                    if chunk.len() == 2 {
                        Some((chunk[0].clone(), chunk[1].clone()))
                    } else {
                        None
                    }
                })
                .collect(),
            body,
        };
        match HttpClientHost::send(self, req, None) {
            Ok(resp) => Ok(resp.body.unwrap_or_default()),
            Err(err) => Err(err.message),
        }
    }
}

impl RunnerHostKv for HostState {
    fn get(&mut self, _ns: String, _key: String) -> Option<String> {
        None
    }

    fn put(&mut self, _ns: String, _key: String, _val: String) {}
}
