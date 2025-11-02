#![cfg(feature = "new-runner")]

use std::sync::Arc;

use async_trait::async_trait;
use greentic_runner::glue::{FnSecretsHost, FnTelemetryHost};
use greentic_runner::newrunner::api::{RunFlowRequest, RunFlowResult};
use greentic_runner::newrunner::builder::RunnerBuilder;
use greentic_runner::newrunner::host::HostBundle;
use greentic_runner::newrunner::policy::{Policy, RetryPolicy};
use greentic_runner::newrunner::registry::{Adapter, AdapterCall, AdapterRegistry};
use greentic_runner::newrunner::shims::{InMemorySessionHost, InMemoryStateHost};
use greentic_runner::newrunner::state_machine::{FlowDefinition, FlowStep};
use greentic_runner::newrunner::{FlowSummary, RunnerError};
use greentic_types::{EnvId, TenantCtx, TenantId};
use serde_json::json;
use tokio::sync::Mutex;

fn tenant() -> TenantCtx {
    TenantCtx {
        env: EnvId::from("dev"),
        tenant: TenantId::from("acme"),
        team: None,
        user: None,
        trace_id: None,
        correlation_id: None,
        deadline: None,
        attempt: 0,
        idempotency_key: None,
    }
}

fn host_bundle() -> HostBundle {
    let secrets = Arc::new(FnSecretsHost::new(|_| Ok("secret".to_string())));
    let telemetry = Arc::new(FnTelemetryHost::new(|_, _| Ok(())));
    let session = Arc::new(InMemorySessionHost::new());
    let state = Arc::new(InMemoryStateHost::new());
    HostBundle::new(secrets, telemetry, session, state)
}

fn sample_flow() -> FlowDefinition {
    FlowDefinition::new(
        FlowSummary {
            id: "flow.test".into(),
            name: "Test".into(),
            version: "1.0.0".into(),
            description: Some("sample".into()),
        },
        json!({"type": "object"}),
        vec![FlowStep::Adapter(AdapterCall {
            adapter: "demo.adapter".into(),
            operation: "run".into(),
            payload: json!({"value": 1}),
        })],
    )
}

struct MockAdapter {
    counter: Mutex<u32>,
    fail_until: Mutex<u32>,
}

impl MockAdapter {
    fn new(failures: u32) -> Self {
        Self {
            counter: Mutex::new(0),
            fail_until: Mutex::new(failures),
        }
    }

    async fn call_count(&self) -> u32 {
        *self.counter.lock().await
    }
}

#[async_trait]
impl Adapter for Arc<MockAdapter> {
    async fn call(&self, _call: &AdapterCall) -> Result<serde_json::Value, RunnerError> {
        let mut count = self.counter.lock().await;
        *count += 1;
        let mut fail = self.fail_until.lock().await;
        if *fail > 0 {
            *fail -= 1;
            return Err(RunnerError::AdapterCall {
                reason: "transient".into(),
            });
        }
        Ok(json!({"status": "ok", "call": *count}))
    }
}

#[tokio::test]
async fn adapter_flow_runs_and_returns_response() {
    let mut adapters = AdapterRegistry::default();
    adapters.register_arc("demo.adapter", Arc::new(MockAdapter::new(0)));

    let mut runner = RunnerBuilder::new()
        .with_host(host_bundle())
        .with_adapters(adapters)
        .with_policy(Policy::default())
        .with_flow(sample_flow())
        .build()
        .expect("runner builds");

    let result: RunFlowResult = runner
        .run_flow(RunFlowRequest {
            tenant: tenant(),
            flow_id: "flow.test".into(),
            input: json!({}),
            session_hint: None,
        })
        .await
        .expect("flow succeeds");

    assert_eq!(result.outcome["status"], "done");
    assert_eq!(result.outcome["response"]["status"], "ok");
}

#[tokio::test]
async fn retry_applies_until_success() {
    let adapter = Arc::new(MockAdapter::new(2));
    let mut adapters = AdapterRegistry::default();
    adapters.register_arc("demo.adapter", adapter.clone());

    let policy = Policy {
        retry: RetryPolicy {
            max_attempts: 5,
            initial_backoff: std::time::Duration::from_millis(5),
            max_backoff: std::time::Duration::from_millis(10),
        },
        ..Policy::default()
    };

    let mut runner = RunnerBuilder::new()
        .with_host(host_bundle())
        .with_adapters(adapters)
        .with_policy(policy)
        .with_flow(sample_flow())
        .build()
        .unwrap();

    let result = runner
        .run_flow(RunFlowRequest {
            tenant: tenant(),
            flow_id: "flow.test".into(),
            input: json!({}),
            session_hint: None,
        })
        .await
        .unwrap();

    assert_eq!(result.outcome["response"]["status"], "ok");
    assert!(adapter.call_count().await >= 3);
}

#[tokio::test]
async fn idempotent_outbox_skips_duplicate() {
    let adapter = Arc::new(MockAdapter::new(0));
    let mut adapters = AdapterRegistry::default();
    adapters.register_arc("demo.adapter", adapter.clone());

    let host = host_bundle();
    let mut runner = RunnerBuilder::new()
        .with_host(host)
        .with_adapters(adapters)
        .with_policy(Policy::default())
        .with_flow(sample_flow())
        .build()
        .unwrap();

    let request = RunFlowRequest {
        tenant: tenant(),
        flow_id: "flow.test".into(),
        input: json!({}),
        session_hint: Some("session-a".into()),
    };

    runner.run_flow(request.clone()).await.unwrap();
    runner.run_flow(request).await.unwrap();

    assert_eq!(adapter.call_count().await, 1);
}
