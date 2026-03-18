//! Component state and linker registration for WASM execution.

use super::host_state::HostState;
use crate::component_api;
use crate::oauth::{OAuthBrokerConfig, OAuthBrokerHost, OAuthHostContext};
use crate::runtime_wasmtime::{Linker, ResourceTable};
use crate::wasi::RunnerWasiPolicy;
use anyhow::{Context, Result};
use greentic_interfaces_wasmtime::host_helpers::v1::http_client as host_http_client;
use greentic_interfaces_wasmtime::host_helpers::v1::{
    self as host_v1, HostFns, add_all_v1_to_linker,
};
use greentic_interfaces_wasmtime::http_client_client_v1_1::greentic::http::http_client as http_client_client_alias;
use greentic_interfaces_wasmtime::http_client_client_v1_1::greentic::interfaces_types::types as http_types_v1_1;
use std::sync::Arc;
use wasmtime::StoreContextMut;
use wasmtime_wasi::p2::add_to_linker_sync as add_wasi_to_linker;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

/// State for a WASM component instance.
pub struct ComponentState {
    pub host: HostState,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

impl ComponentState {
    /// Create a new component state.
    pub fn new(host: HostState, policy: Arc<RunnerWasiPolicy>) -> Result<Self> {
        let wasi_ctx = policy
            .instantiate()
            .context("failed to build WASI context")?;
        Ok(Self {
            host,
            wasi_ctx,
            resource_table: ResourceTable::new(),
        })
    }

    pub(crate) fn host_mut(&mut self) -> &mut HostState {
        &mut self.host
    }

    fn should_cancel_host(&mut self) -> bool {
        false
    }

    fn yield_now_host(&mut self) {
        // no-op cooperative yield
    }
}

impl component_api::v0_4::greentic::component::control::Host for ComponentState {
    fn should_cancel(&mut self) -> bool {
        self.should_cancel_host()
    }

    fn yield_now(&mut self) {
        self.yield_now_host();
    }
}

impl component_api::v0_5::greentic::component::control::Host for ComponentState {
    fn should_cancel(&mut self) -> bool {
        self.should_cancel_host()
    }

    fn yield_now(&mut self) {
        self.yield_now_host();
    }
}

impl OAuthHostContext for ComponentState {
    fn tenant_id(&self) -> &str {
        &self.host.config.tenant
    }

    fn env(&self) -> &str {
        &self.host.default_env
    }

    fn oauth_broker_host(&mut self) -> &mut OAuthBrokerHost {
        &mut self.host.oauth_host
    }

    fn oauth_config(&self) -> Option<&OAuthBrokerConfig> {
        self.host.oauth_config.as_ref()
    }
}

impl WasiView for ComponentState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resource_table,
        }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for ComponentState {}
#[allow(unsafe_code)]
unsafe impl Sync for ComponentState {}

/// Register all host functions with the linker.
pub fn register_all(linker: &mut Linker<ComponentState>, allow_state_store: bool) -> Result<()> {
    add_wasi_to_linker(linker)?;
    add_all_v1_to_linker(
        linker,
        HostFns {
            http_client_v1_1: Some(|state: &mut ComponentState| state.host_mut()),
            http_client: Some(|state: &mut ComponentState| state.host_mut()),
            oauth_broker: None,
            runner_host_http: Some(|state: &mut ComponentState| state.host_mut()),
            runner_host_kv: Some(|state: &mut ComponentState| state.host_mut()),
            telemetry_logger: Some(|state: &mut ComponentState| state.host_mut()),
            state_store: allow_state_store.then_some(|state: &mut ComponentState| state.host_mut()),
            secrets_store_v1_1: Some(|state: &mut ComponentState| state.host_mut()),
            secrets_store: None,
        },
    )?;
    add_http_client_client_world_aliases(linker)?;
    Ok(())
}

/// Add component control functions to linker.
pub fn add_component_control_to_linker(
    linker: &mut Linker<ComponentState>,
) -> wasmtime::Result<()> {
    add_component_control_instance(linker, "greentic:component/control@0.5.0")?;
    add_component_control_instance(linker, "greentic:component/control@0.4.0")?;
    Ok(())
}

fn add_component_control_instance(
    linker: &mut Linker<ComponentState>,
    name: &str,
) -> wasmtime::Result<()> {
    let mut inst = linker.instance(name)?;
    inst.func_wrap(
        "should-cancel",
        |mut caller: StoreContextMut<'_, ComponentState>, (): ()| {
            let host = caller.data_mut();
            Ok((host.should_cancel_host(),))
        },
    )?;
    inst.func_wrap(
        "yield-now",
        |mut caller: StoreContextMut<'_, ComponentState>, (): ()| {
            let host = caller.data_mut();
            host.yield_now_host();
            Ok(())
        },
    )?;
    Ok(())
}

fn add_http_client_client_world_aliases(linker: &mut Linker<ComponentState>) -> Result<()> {
    let mut inst_v1_1 = linker.instance("greentic:http/client@1.1.0")?;
    inst_v1_1.func_wrap(
        "send",
        move |mut caller: StoreContextMut<'_, ComponentState>,
              (req, opts, ctx): (
            http_client_client_alias::Request,
            Option<http_client_client_alias::RequestOptions>,
            Option<http_client_client_alias::TenantCtx>,
        )| {
            let host = caller.data_mut().host_mut();
            let result = host_v1::http_client::HttpClientHostV1_1::send(
                host,
                alias_request_to_host(req),
                opts.map(alias_request_options_to_host),
                ctx.map(alias_tenant_ctx_to_host),
            );
            Ok((match result {
                Ok(resp) => Ok(alias_response_from_host(resp)),
                Err(err) => Err(alias_error_from_host(err)),
            },))
        },
    )?;
    let mut inst_v1_0 = linker.instance("greentic:http/client@1.0.0")?;
    inst_v1_0.func_wrap(
        "send",
        move |mut caller: StoreContextMut<'_, ComponentState>,
              (req, ctx): (
            host_http_client::Request,
            Option<host_http_client::TenantCtx>,
        )| {
            let host = caller.data_mut().host_mut();
            let result = host_v1::http_client::HttpClientHost::send(host, req, ctx);
            Ok((result,))
        },
    )?;
    Ok(())
}

fn alias_request_to_host(req: http_client_client_alias::Request) -> host_http_client::RequestV1_1 {
    host_http_client::RequestV1_1 {
        method: req.method,
        url: req.url,
        headers: req.headers,
        body: req.body,
    }
}

fn alias_request_options_to_host(
    opts: http_client_client_alias::RequestOptions,
) -> host_http_client::RequestOptionsV1_1 {
    host_http_client::RequestOptionsV1_1 {
        timeout_ms: opts.timeout_ms,
        allow_insecure: opts.allow_insecure,
        follow_redirects: opts.follow_redirects,
    }
}

fn alias_tenant_ctx_to_host(
    ctx: http_client_client_alias::TenantCtx,
) -> host_http_client::TenantCtxV1_1 {
    host_http_client::TenantCtxV1_1 {
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
        impersonation: ctx.impersonation.map(|imp| http_types_v1_1::Impersonation {
            actor_id: imp.actor_id,
            reason: imp.reason,
        }),
    }
}

fn alias_response_from_host(
    resp: host_http_client::ResponseV1_1,
) -> http_client_client_alias::Response {
    http_client_client_alias::Response {
        status: resp.status,
        headers: resp.headers,
        body: resp.body,
    }
}

fn alias_error_from_host(
    err: host_http_client::HttpClientErrorV1_1,
) -> http_client_client_alias::HostError {
    http_client_client_alias::HostError {
        code: err.code,
        message: err.message,
    }
}
