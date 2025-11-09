#![forbid(unsafe_code)]

pub use greentic_runner_host::{
    self as host, Activity, ActivityKind, HostBuilder, HostServer, RunnerHost, TenantHandle,
    config, imports, pack, runner, runtime_wasmtime, telemetry, verify,
};

pub mod desktop {
    pub use greentic_runner_desktop::*;
}

#[cfg(feature = "new-runner")]
pub mod newrunner {
    pub use greentic_runner_new::{
        Adapter, AdapterCall, AdapterRegistry, FlowSchema, FlowSummary, GResult, Policy,
        RetryPolicy, RunFlowRequest, RunFlowResult, Runner, RunnerApi, RunnerBuilder, RunnerError,
        SessionKey, SessionSnapshot, api, builder, error, glue, host, policy, registry, shims,
        state_machine,
    };
}

#[cfg(feature = "new-runner")]
pub mod glue {
    pub use greentic_runner_new::glue::*;
}
