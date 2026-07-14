use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use greentic_session::{SessionData, SessionKey as StoreSessionKey};
use greentic_types::{
    EnvId, FlowId, GreenticError, PackId, ReplyScope, SessionCursor as TypesSessionCursor,
    TenantCtx, TenantId, UserId,
};
use rand::{RngExt, rng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::api::{RunFlowRequest, RunnerApi};
use super::builder::{Runner, RunnerBuilder};
use super::error::{GResult, RunnerError};
use super::glue::{FnSecretsHost, FnTelemetryHost};
use super::host::{HostBundle, SecretsHost, SessionHost, StateHost};
use super::policy::Policy;
use super::registry::{Adapter, AdapterCall, AdapterRegistry};
use super::shims::{InMemorySessionHost, InMemoryStateHost};
use super::state_machine::{FlowDefinition, FlowStep, PAYLOAD_FROM_LAST_INPUT};

use crate::config::{HostConfig, SecretsPolicy};
use crate::pack::FlowDescriptor;
use crate::runner::engine::{
    FlowContext, FlowEngine, FlowExecution, FlowSnapshot, FlowStatus, FlowWait,
};
use crate::runner::mocks::MockLayer;
use crate::secrets::{DynSecretsManager, read_secret_blocking};
use crate::storage::session::DynSessionStore;
use crate::trace::audit_sink::AuditSink;
use crate::trace::{PackTraceInfo, TraceContext, TraceMode, TraceRecorder};

const DEFAULT_ENV: &str = "local";
const PACK_FLOW_ADAPTER: &str = "pack_flow";

#[derive(Clone)]
pub struct FlowResumeStore {
    store: DynSessionStore,
}

impl FlowResumeStore {
    pub fn new(store: DynSessionStore) -> Self {
        Self { store }
    }

    pub fn fetch(&self, envelope: &IngressEnvelope) -> GResult<Option<FlowSnapshot>> {
        let (mut ctx, user, _, scope) = build_store_ctx(envelope)?;
        ctx = ctx.with_user(Some(user.clone()));

        let mut scopes = vec![scope.clone()];
        if scope.correlation.is_some() {
            let mut base = scope.clone();
            base.correlation = None;
            scopes.push(base);
        }

        for lookup in scopes {
            if let Some(key) = self
                .store
                .find_wait_by_scope(&ctx, &user, &lookup)
                .map_err(map_store_error)?
            {
                let Some(data) = self.store.get_session(&key).map_err(map_store_error)? else {
                    continue;
                };
                let record: FlowResumeRecord =
                    serde_json::from_str(&data.context_json).map_err(|err| {
                        RunnerError::Session {
                            reason: format!("failed to decode flow resume snapshot: {err}"),
                        }
                    })?;
                if let Some(pack_id) = envelope.pack_id.as_deref()
                    && record.snapshot.pack_id != pack_id
                {
                    return Err(RunnerError::Session {
                        reason: format!(
                            "resume pack mismatch: expected {pack_id}, found {}",
                            record.snapshot.pack_id
                        ),
                    });
                }
                return Ok(Some(record.snapshot));
            }
        }

        Ok(None)
    }

    pub fn save(&self, envelope: &IngressEnvelope, wait: &FlowWait) -> GResult<ReplyScope> {
        let (ctx, user, hint, scope) = build_store_ctx(envelope)?;
        let record = FlowResumeRecord {
            snapshot: wait.snapshot.clone(),
            reason: wait.reason.clone(),
        };
        let data = record_to_session_data(&record, ctx.clone(), &user, &hint)?;
        let mut reply_scope = scope.clone();
        if reply_scope.correlation.is_none() {
            reply_scope.correlation = Some(generate_correlation_id());
        }
        let mut store_scope = scope;
        store_scope.correlation = None;
        let session_key = StoreSessionKey::new(format!("{hint}::{}", store_scope.scope_hash()));
        self.store
            .register_wait(&ctx, &user, &store_scope, &session_key, data, None)
            .map_err(map_store_error)?;
        Ok(reply_scope)
    }

    pub fn clear(&self, envelope: &IngressEnvelope) -> GResult<()> {
        let (ctx, user, _, scope) = build_store_ctx(envelope)?;
        let mut scopes = vec![scope.clone()];
        if scope.correlation.is_some() {
            let mut base = scope;
            base.correlation = None;
            scopes.push(base);
        }
        for lookup in scopes {
            self.store
                .clear_wait(&ctx, &user, &lookup)
                .map_err(map_store_error)?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct FlowResumeRecord {
    snapshot: FlowSnapshot,
    #[serde(default)]
    reason: Option<String>,
}

fn build_store_ctx(envelope: &IngressEnvelope) -> GResult<(TenantCtx, UserId, String, ReplyScope)> {
    let base_hint = envelope
        .session_hint
        .clone()
        .unwrap_or_else(|| envelope.canonical_session_hint());
    let hint = if let Some(pack_id) = envelope.pack_id.as_deref() {
        format!("{base_hint}::pack={pack_id}")
    } else {
        base_hint.clone()
    };
    let user = derive_user_id(&hint)?;
    let scope = envelope
        .reply_scope
        .clone()
        .ok_or_else(|| RunnerError::Session {
            reason: "Cannot suspend: reply_scope missing; provider plugin must supply ReplyScope"
                .to_string(),
        })?;
    let mut ctx = envelope.tenant_ctx();
    ctx = ctx.with_session(hint.clone());
    ctx = ctx.with_user(Some(user.clone()));
    Ok((ctx, user, hint, scope))
}

fn record_to_session_data(
    record: &FlowResumeRecord,
    ctx: TenantCtx,
    user: &UserId,
    session_hint: &str,
) -> GResult<SessionData> {
    let flow = FlowId::from_str(record.snapshot.flow_id.as_str()).map_err(map_store_error)?;
    let pack = PackId::from_str(record.snapshot.pack_id.as_str()).map_err(map_store_error)?;
    let mut cursor = TypesSessionCursor::new(record.snapshot.next_node.clone());
    if let Some(reason) = record.reason.clone() {
        cursor = cursor.with_wait_reason(reason);
    }
    let context_json = serde_json::to_string(record).map_err(|err| RunnerError::Session {
        reason: format!("failed to encode flow resume snapshot: {err}"),
    })?;
    let ctx = ctx
        .with_user(Some(user.clone()))
        .with_session(session_hint.to_string())
        .with_flow(record.snapshot.flow_id.clone());
    Ok(SessionData {
        tenant_ctx: ctx,
        flow_id: flow,
        pack_id: Some(pack),
        cursor,
        context_json,
    })
}

fn derive_user_id(hint: &str) -> GResult<UserId> {
    let digest = Sha256::digest(hint.as_bytes());
    let slug = format!("sess{}", hex::encode(&digest[..8]));
    UserId::from_str(&slug).map_err(map_store_error)
}

fn map_store_error(err: GreenticError) -> RunnerError {
    RunnerError::Session {
        reason: err.to_string(),
    }
}

fn generate_correlation_id() -> String {
    let mut bytes = [0u8; 16];
    rng().fill(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::engine::ExecutionState;
    use crate::storage::session::new_session_store;
    use serde_json::json;

    fn sample_envelope() -> IngressEnvelope {
        IngressEnvelope {
            tenant: "demo".into(),
            env: Some("local".into()),
            pack_id: Some("pack.demo".into()),
            flow_id: "flow.main".into(),
            flow_type: None,
            action: Some("messaging".into()),
            session_hint: Some("demo:provider:chan:conv:user".into()),
            provider: Some("provider".into()),
            channel: Some("chan".into()),
            conversation: Some("conv".into()),
            user: Some("user".into()),
            activity_id: Some("act-1".into()),
            timestamp: None,
            payload: json!({ "text": "hi" }),
            metadata: None,
            reply_scope: Some(ReplyScope {
                conversation: "conv".into(),
                thread: None,
                reply_to: None,
                correlation: None,
            }),
        }
    }

    fn sample_wait() -> FlowWait {
        let state: ExecutionState = serde_json::from_value(json!({
            "input": { "text": "hi" },
            "nodes": {},
            "egress": []
        }))
        .expect("state");
        FlowWait {
            reason: Some("await-user".into()),
            snapshot: FlowSnapshot {
                pack_id: "pack.demo".into(),
                flow_id: "flow.main".into(),
                next_flow: None,
                next_node: "node-2".into(),
                state,
            },
        }
    }

    #[test]
    fn derive_user_id_is_stable() {
        let hint = "some-tenant::session-key";
        let a = derive_user_id(hint).unwrap();
        let b = derive_user_id(hint).unwrap();
        assert_eq!(a, b);
        assert!(a.as_str().starts_with("sess"));
    }

    #[test]
    fn resume_store_roundtrip() -> GResult<()> {
        let store = FlowResumeStore::new(new_session_store());
        let envelope = sample_envelope();
        assert!(store.fetch(&envelope)?.is_none());

        let wait = sample_wait();
        let _ = store.save(&envelope, &wait)?;
        let snapshot = store.fetch(&envelope)?.expect("snapshot missing");
        assert_eq!(snapshot.flow_id, wait.snapshot.flow_id);
        assert_eq!(snapshot.next_node, wait.snapshot.next_node);

        store.clear(&envelope)?;
        assert!(store.fetch(&envelope)?.is_none());
        Ok(())
    }

    #[test]
    fn resume_store_overwrites_existing() -> GResult<()> {
        let store = FlowResumeStore::new(new_session_store());
        let envelope = sample_envelope();
        let mut wait = sample_wait();
        let _ = store.save(&envelope, &wait)?;

        wait.snapshot.next_node = "node-3".into();
        wait.reason = Some("retry".into());
        let _ = store.save(&envelope, &wait)?;

        let snapshot = store.fetch(&envelope)?.expect("snapshot missing");
        assert_eq!(snapshot.next_node, "node-3");
        store.clear(&envelope)?;
        Ok(())
    }

    #[test]
    fn resume_store_uses_snapshot_even_if_envelope_flow_differs() -> GResult<()> {
        let store = FlowResumeStore::new(new_session_store());
        let envelope = sample_envelope();
        let wait = sample_wait();
        let _ = store.save(&envelope, &wait)?;

        let mut redirected = envelope.clone();
        redirected.flow_id = "flow.other".into();
        let snapshot = store.fetch(&redirected)?.expect("snapshot missing");
        assert_eq!(snapshot.flow_id, wait.snapshot.flow_id);

        store.clear(&envelope)?;
        Ok(())
    }

    #[test]
    fn canonicalize_populates_defaults() {
        let envelope = IngressEnvelope {
            tenant: "demo".into(),
            env: None,
            pack_id: None,
            flow_id: "flow.main".into(),
            flow_type: None,
            action: None,
            session_hint: None,
            provider: None,
            channel: None,
            conversation: None,
            user: None,
            activity_id: Some("activity-1".into()),
            timestamp: None,
            payload: json!({}),
            metadata: None,
            reply_scope: None,
        }
        .canonicalize();

        assert_eq!(envelope.provider.as_deref(), Some("provider"));
        assert_eq!(envelope.channel.as_deref(), Some("flow.main"));
        assert_eq!(envelope.conversation.as_deref(), Some("flow.main"));
        assert_eq!(envelope.user.as_deref(), Some("activity-1"));
        assert!(envelope.session_hint.is_some());
    }
}

pub struct StateMachineRuntime {
    runner: Runner,
}

impl StateMachineRuntime {
    /// Construct a runtime from explicit flow definitions (legacy entrypoint used by tests/examples).
    pub fn new(flows: Vec<FlowDefinition>) -> GResult<Self> {
        let secrets = Arc::new(FnSecretsHost::new(|name| {
            Err(RunnerError::Secrets {
                reason: format!("secret {name} unavailable (noop host)"),
            })
        }));
        let telemetry = Arc::new(FnTelemetryHost::new(|_, _| Ok(())));
        let session = Arc::new(InMemorySessionHost::new());
        let state = Arc::new(InMemoryStateHost::new());
        let host = HostBundle::new(secrets, telemetry, session, state);

        let adapters = AdapterRegistry::default();
        let policy = Policy::default();

        let mut builder = RunnerBuilder::new()
            .with_host(host)
            .with_adapters(adapters)
            .with_policy(policy);
        for flow in flows {
            builder = builder.with_flow(flow);
        }
        let runner = builder.build()?;
        Ok(Self { runner })
    }

    /// Build a state-machine runtime that proxies pack flows through the legacy FlowEngine.
    #[allow(clippy::too_many_arguments)]
    pub fn from_flow_engine(
        config: Arc<HostConfig>,
        engine: Arc<FlowEngine>,
        pack_trace: HashMap<String, PackTraceInfo>,
        session_host: Arc<dyn SessionHost>,
        session_store: DynSessionStore,
        state_host: Arc<dyn StateHost>,
        secrets_manager: DynSecretsManager,
        mocks: Option<Arc<MockLayer>>,
        audit_nats_client: Option<async_nats::Client>,
    ) -> Result<Self> {
        let policy = Arc::new(config.secrets_policy.clone());
        let tenant_ctx = config.tenant_ctx();
        let secrets = Arc::new(PolicySecretsHost::new(policy, secrets_manager, tenant_ctx));
        let telemetry = Arc::new(FnTelemetryHost::new(|span, fields| {
            tracing::debug!(?span, ?fields, "telemetry emit");
            Ok(())
        }));
        let host = HostBundle::new(secrets, telemetry, session_host, state_host);
        let resume_store = FlowResumeStore::new(session_store);

        let mut adapters = AdapterRegistry::default();
        adapters.register(
            PACK_FLOW_ADAPTER,
            Box::new(PackFlowAdapter::new(
                Arc::clone(&config),
                Arc::clone(&engine),
                pack_trace,
                resume_store,
                mocks,
                audit_nats_client,
            )),
        );

        let flows = build_flow_definitions(engine.flows());
        let mut builder = RunnerBuilder::new()
            .with_host(host)
            .with_adapters(adapters)
            .with_policy(Policy::default());
        for flow in flows {
            builder = builder.with_flow(flow);
        }
        let runner = builder
            .build()
            .map_err(|err| anyhow!("state machine init failed: {err}"))?;
        Ok(Self { runner })
    }

    /// Execute the flow associated with the provided ingress event.
    pub async fn handle(&self, envelope: IngressEnvelope) -> Result<Value> {
        let result = self.run_flow_result(envelope).await?;
        let outcome = result.outcome;
        Ok(outcome.get("response").cloned().unwrap_or(outcome))
    }

    /// Like [`Self::handle`], but also returns this turn's per-step node-output
    /// map (keyed by each step's `operation`; for a `dw.agent` step that is the
    /// agent_id). Observability only — the returned response `Value` is computed
    /// byte-identically to [`Self::handle`].
    pub async fn handle_traced(&self, envelope: IngressEnvelope) -> Result<(Value, Value)> {
        let result = self.run_flow_result(envelope).await?;
        let node_outputs = result.node_outputs;
        let outcome = result.outcome;
        let response = outcome.get("response").cloned().unwrap_or(outcome);
        Ok((response, node_outputs))
    }

    /// Shared body for [`Self::handle`] / [`Self::handle_traced`]: build the
    /// ingress request and run the flow. Keeping this single seam guarantees the
    /// traced and untraced paths can never diverge on the outcome.
    async fn run_flow_result(
        &self,
        envelope: IngressEnvelope,
    ) -> Result<super::api::RunFlowResult> {
        let tenant_ctx = envelope.tenant_ctx();
        let session_hint = envelope
            .session_hint
            .clone()
            .unwrap_or_else(|| envelope.canonical_session_hint());
        let pack_id = envelope.pack_id.clone().ok_or_else(|| {
            anyhow!("pack_id missing; ingress must specify pack_id for multi-pack flows")
        })?;
        let input =
            serde_json::to_value(&envelope).context("failed to serialise ingress envelope")?;
        let request = RunFlowRequest {
            tenant: tenant_ctx,
            pack_id,
            flow_id: envelope.flow_id.clone(),
            input,
            session_hint: Some(session_hint),
        };
        self.runner
            .run_flow(request)
            .await
            .map_err(|err| anyhow!("flow execution failed: {err}"))
    }
}

struct PolicySecretsHost {
    policy: Arc<SecretsPolicy>,
    manager: DynSecretsManager,
    tenant_ctx: TenantCtx,
}

impl PolicySecretsHost {
    fn new(policy: Arc<SecretsPolicy>, manager: DynSecretsManager, tenant_ctx: TenantCtx) -> Self {
        Self {
            policy,
            manager,
            tenant_ctx,
        }
    }
}

const POLICY_SECRETS_PACK_ID: &str = "_runner";

#[async_trait]
impl SecretsHost for PolicySecretsHost {
    async fn get(&self, name: &str) -> GResult<String> {
        if !self.policy.is_allowed(name) {
            return Err(RunnerError::Secrets {
                reason: format!("secret {name} denied by policy"),
            });
        }
        let bytes = read_secret_blocking(
            &self.manager,
            &self.tenant_ctx,
            POLICY_SECRETS_PACK_ID,
            name,
        )
        .map_err(|err| RunnerError::Secrets {
            reason: format!("secret {name} unavailable: {err}"),
        })?;
        String::from_utf8(bytes).map_err(|err| RunnerError::Secrets {
            reason: format!("secret {name} not valid UTF-8: {err}"),
        })
    }
}

fn build_flow_definitions(flows: &[FlowDescriptor]) -> Vec<FlowDefinition> {
    flows
        .iter()
        .map(|descriptor| {
            FlowDefinition::new(
                super::api::FlowSummary {
                    pack_id: descriptor.pack_id.clone(),
                    id: descriptor.id.clone(),
                    name: descriptor
                        .description
                        .clone()
                        .unwrap_or_else(|| descriptor.id.clone()),
                    version: descriptor.version.clone(),
                    description: descriptor.description.clone(),
                },
                serde_json::json!({
                    "type": "object"
                }),
                vec![FlowStep::Adapter(AdapterCall {
                    adapter: PACK_FLOW_ADAPTER.into(),
                    operation: descriptor.id.clone(),
                    payload: Value::String(PAYLOAD_FROM_LAST_INPUT.into()),
                })],
            )
        })
        .collect()
}

struct PackFlowAdapter {
    tenant: String,
    config: Arc<HostConfig>,
    engine: Arc<FlowEngine>,
    pack_trace: HashMap<String, PackTraceInfo>,
    resume: FlowResumeStore,
    mocks: Option<Arc<MockLayer>>,
    /// NATS client backing the per-node audit-event sink; `None` (the
    /// default, off path) when `GREENTIC_EVENTS_NATS_URL` is unset or NATS
    /// could not be reached — see `docs/superpowers/specs/2026-07-03-runner-audit-emitter-design.md`.
    audit_nats_client: Option<async_nats::Client>,
}

impl PackFlowAdapter {
    fn new(
        config: Arc<HostConfig>,
        engine: Arc<FlowEngine>,
        pack_trace: HashMap<String, PackTraceInfo>,
        resume: FlowResumeStore,
        mocks: Option<Arc<MockLayer>>,
        audit_nats_client: Option<async_nats::Client>,
    ) -> Self {
        Self {
            tenant: config.tenant.clone(),
            config,
            engine,
            pack_trace,
            resume,
            mocks,
            audit_nats_client,
        }
    }
}

#[async_trait::async_trait]
impl Adapter for PackFlowAdapter {
    async fn call(&self, call: &AdapterCall) -> GResult<Value> {
        Ok(self.call_traced(call).await?.0)
    }

    async fn call_traced(
        &self,
        call: &AdapterCall,
    ) -> GResult<(Value, serde_json::Map<String, Value>)> {
        let envelope: IngressEnvelope =
            serde_json::from_value(call.payload.clone()).map_err(|err| {
                RunnerError::AdapterCall {
                    reason: format!("invalid ingress payload: {err}"),
                }
            })?;
        let envelope = envelope.canonicalize();
        let flow_id = call.operation.clone();
        let action_owned = envelope.action.clone();
        let session_owned = envelope
            .session_hint
            .clone()
            .unwrap_or_else(|| envelope.canonical_session_hint());
        let provider_owned = envelope.provider.clone();
        let payload = envelope.payload.clone();
        let retry_config = self.config.retry_config().into();
        let resume_snapshot = self.resume.fetch(&envelope)?;
        let resume_flow_id = resume_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.next_flow.clone())
            .or_else(|| {
                resume_snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.flow_id.clone())
            });
        let effective_flow_id = resume_flow_id.clone().unwrap_or_else(|| flow_id.clone());
        let effective_pack_id = if let Some(snapshot) = resume_snapshot.as_ref() {
            snapshot.pack_id.clone()
        } else if let Some(pack_id) = envelope.pack_id.as_deref() {
            let found = self
                .engine
                .flow_by_key(pack_id, effective_flow_id.as_str())
                .is_some();
            if !found {
                return Err(RunnerError::AdapterCall {
                    reason: format!(
                        "flow {} not registered for pack {pack_id}",
                        effective_flow_id
                    ),
                });
            }
            pack_id.to_string()
        } else if let Some(flow) = self.engine.flow_by_id(effective_flow_id.as_str()) {
            flow.pack_id.clone()
        } else {
            return Err(RunnerError::AdapterCall {
                reason: format!(
                    "flow {} is ambiguous; pack_id is required",
                    effective_flow_id
                ),
            });
        };

        let trace_config = self.config.trace.clone();
        let flow_version = self
            .engine
            .flow_by_key(effective_pack_id.as_str(), effective_flow_id.as_str())
            .map(|desc| desc.version.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let pack_trace = self
            .pack_trace
            .get(effective_pack_id.as_str())
            .cloned()
            .unwrap_or_else(|| PackTraceInfo {
                pack_ref: effective_pack_id.clone(),
                resolved_digest: None,
            });
        let trace_ctx = TraceContext {
            pack_ref: pack_trace.pack_ref,
            resolved_digest: pack_trace.resolved_digest,
            flow_id: effective_flow_id.clone(),
            flow_version,
        };
        let trace = if trace_config.mode == TraceMode::Off {
            None
        } else {
            // Build the best-effort audit sink from the threaded NATS client
            // (EPIC-B B-2). `audit_tenant` is derived from the same
            // condition as the sink so the two stay coupled: both `Some`
            // (audit enabled) or both `None` (default, file-trace-only path,
            // zero behaviour change). `TraceRecorder` construction happens
            // on the async execution path, so `AuditSink::new`'s
            // `tokio::spawn` runs inside a live tokio runtime context.
            let sink = self.audit_nats_client.clone().map(AuditSink::new);
            let audit_tenant = sink.is_some().then(|| envelope.tenant_ctx());
            Some(TraceRecorder::new_with_audit(
                trace_config,
                trace_ctx,
                sink,
                audit_tenant,
            ))
        };

        let mocks = self.mocks.as_deref();
        let ctx = FlowContext {
            tenant: &self.tenant,
            pack_id: effective_pack_id.as_str(),
            flow_id: effective_flow_id.as_str(),
            node_id: None,
            tool: None,
            action: action_owned.as_deref(),
            session_id: Some(session_owned.as_str()),
            provider_id: provider_owned.as_deref(),
            // Carry the inbound reply scope so async-dispatch nodes can encode
            // the originating thread/reply_to into the dispatch correlation id
            // (so a threaded wait can be re-keyed on resume).
            reply_scope: envelope.reply_scope.as_ref(),
            retry_config,
            attempt: 1,
            observer: trace
                .as_ref()
                .map(|recorder| recorder as &dyn crate::runner::engine::ExecutionObserver),
            mocks,
        };

        let execution = if let Some(snapshot) = resume_snapshot {
            let resume_pack_id = snapshot.pack_id.clone();
            let resume_flow_id = snapshot
                .next_flow
                .clone()
                .unwrap_or_else(|| snapshot.flow_id.clone());
            let resume_ctx = FlowContext {
                pack_id: resume_pack_id.as_str(),
                flow_id: resume_flow_id.as_str(),
                ..ctx
            };
            self.engine.resume(resume_ctx, snapshot, payload).await
        } else {
            self.engine.execute(ctx, payload).await
        };
        let execution = match execution {
            Ok(execution) => {
                if let Some(recorder) = trace.as_ref()
                    && let Err(err) = recorder.flush_success()
                {
                    tracing::warn!(error = %err, "failed to write trace");
                }
                execution
            }
            Err(err) => {
                if let Some(recorder) = trace.as_ref()
                    && let Err(write_err) = recorder.flush_error(err.as_ref())
                {
                    tracing::warn!(error = %write_err, "failed to write trace");
                }
                return Err(RunnerError::AdapterCall {
                    // `{:#}` flattens the full anyhow source chain; a bare
                    // `to_string()` keeps only the outermost context and drops
                    // the real root cause (e.g. the underlying LLM/dispatch
                    // error behind "in-process operala dispatch … failed").
                    reason: format!("{err:#}"),
                });
            }
        };

        let FlowExecution {
            output,
            status,
            node_outputs,
        } = execution;
        match status {
            FlowStatus::Completed => {
                self.resume.clear(&envelope)?;
                Ok((output, node_outputs))
            }
            FlowStatus::Waiting(wait) => {
                let reply_scope = self.resume.save(&envelope, &wait)?;
                let outcome = json!({
                    "status": "pending",
                    "reason": wait.reason,
                    "resume": wait.snapshot,
                    "reply_scope": reply_scope,
                    "response": output,
                });
                Ok((outcome, node_outputs))
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IngressEnvelope {
    pub tenant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<String>,
    pub flow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_scope: Option<ReplyScope>,
}

impl IngressEnvelope {
    pub fn canonicalize(mut self) -> Self {
        if self.provider.is_none() {
            self.provider = Some("provider".into());
        }
        if self.channel.is_none() {
            self.channel = Some(self.flow_id.clone());
        }
        if self.conversation.is_none() {
            self.conversation = self.channel.clone();
        }
        if self.user.is_none() {
            if let Some(ref hint) = self.session_hint {
                self.user = Some(hint.clone());
            } else if let Some(ref activity) = self.activity_id {
                self.user = Some(activity.clone());
            } else {
                self.user = Some("user".into());
            }
        }
        if self.session_hint.is_none() {
            self.session_hint = Some(self.canonical_session_hint());
        }
        if self.reply_scope.is_none()
            && let Some(conversation) = self.conversation.clone()
        {
            self.reply_scope = Some(ReplyScope {
                conversation,
                thread: None,
                reply_to: None,
                correlation: None,
            });
        }
        self
    }

    pub fn canonical_session_hint(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.tenant,
            self.provider.as_deref().unwrap_or("provider"),
            self.channel.as_deref().unwrap_or("channel"),
            self.conversation.as_deref().unwrap_or("conversation"),
            self.user.as_deref().unwrap_or("user")
        )
    }

    pub fn tenant_ctx(&self) -> TenantCtx {
        let env_raw = self.env.clone().unwrap_or_else(|| DEFAULT_ENV.into());
        let env = EnvId::from_str(env_raw.as_str())
            .unwrap_or_else(|_| EnvId::from_str(DEFAULT_ENV).expect("default env must be valid"));
        let tenant_id = TenantId::from_str(self.tenant.as_str()).unwrap_or_else(|_| {
            TenantId::from_str("tenant.default").expect("tenant fallback must be valid")
        });
        let mut ctx = TenantCtx::new(env, tenant_id).with_flow(self.flow_id.clone());
        if let Some(provider) = &self.provider {
            ctx = ctx.with_provider(provider.clone());
        }
        if let Some(session) = &self.session_hint {
            ctx = ctx.with_session(session.clone());
        }
        ctx
    }
}
