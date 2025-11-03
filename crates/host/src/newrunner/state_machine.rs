use crate::newrunner::api::{FlowSchema, FlowSummary};
use crate::newrunner::error::{GResult, RunnerError};
use crate::newrunner::host::{
    HostBundle, OutboxKey, SessionKey, SessionOutboxEntry, SessionSnapshot, SpanContext, WaitState,
};
use crate::newrunner::policy::retry_with_jitter;
use crate::newrunner::policy::{Policy, policy_violation};
use crate::newrunner::registry::{AdapterCall, AdapterRegistry};
use greentic_types::TenantCtx;
use parking_lot::RwLock;
use rand::{Rng, rng};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

const PROVIDER_ID: &str = "greentic-runner";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlowDefinition {
    pub summary: FlowSummary,
    pub schema: Value,
    pub steps: Vec<FlowStep>,
}

impl FlowDefinition {
    pub fn new(summary: FlowSummary, schema: Value, steps: Vec<FlowStep>) -> Self {
        Self {
            summary,
            schema,
            steps,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum FlowStep {
    Adapter(AdapterCall),
    AwaitInput { reason: String },
    Complete { outcome: Value },
}

pub struct StateMachine {
    host: Arc<HostBundle>,
    adapters: AdapterRegistry,
    policy: Policy,
    flows: Arc<RwLock<HashMap<String, FlowDefinition>>>,
}

impl StateMachine {
    pub fn new(host: Arc<HostBundle>, adapters: AdapterRegistry, policy: Policy) -> Self {
        Self {
            host,
            adapters,
            policy,
            flows: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register_flow(&self, definition: FlowDefinition) {
        let mut guard = self.flows.write();
        guard.insert(definition.summary.id.clone(), definition);
    }

    pub fn list_flows(&self) -> Vec<FlowSummary> {
        let guard = self.flows.read();
        guard.values().map(|flow| flow.summary.clone()).collect()
    }

    pub fn get_flow_schema(&self, flow_id: &str) -> GResult<FlowSchema> {
        let guard = self.flows.read();
        guard
            .get(flow_id)
            .map(|flow| FlowSchema {
                id: flow_id.to_string(),
                schema_json: flow.schema.clone(),
            })
            .ok_or_else(|| RunnerError::FlowNotFound {
                flow_id: flow_id.to_string(),
            })
    }

    pub async fn step(
        &self,
        tenant: &TenantCtx,
        flow_id: &str,
        session_hint: Option<String>,
        input: Value,
    ) -> GResult<Value> {
        let mut telemetry_ctx = tenant
            .clone()
            .with_provider(PROVIDER_ID.to_string())
            .with_flow(flow_id.to_string());
        if let Some(hint) = session_hint.as_ref() {
            telemetry_ctx = telemetry_ctx.with_session(hint.clone());
        }
        greentic_types::telemetry::set_current_tenant_ctx(&telemetry_ctx);

        let flow = {
            let guard = self.flows.read();
            guard
                .get(flow_id)
                .cloned()
                .ok_or_else(|| RunnerError::FlowNotFound {
                    flow_id: flow_id.to_string(),
                })?
        };

        self.ensure_policy_budget(&flow)?;

        let key = SessionKey::new(tenant, flow_id, session_hint.clone());
        let session_host = &self.host.session;
        let mut session = match session_host.get(&key).await? {
            Some(snapshot) => snapshot,
            None => {
                let session_id = key
                    .stable_session_id()
                    .unwrap_or_else(Self::generate_session_id);
                SessionSnapshot::new(key.clone(), session_id)
            }
        };
        let is_new = session.revision == 0 && session.outbox.is_empty();
        let expected_revision = session.revision;

        Self::update_state_input(&mut session, input.clone());

        if session.cursor.position >= flow.steps.len() {
            let outcome = session
                .last_outcome
                .clone()
                .unwrap_or_else(|| json!({"status": "done"}));
            return Ok(outcome);
        }

        let step = flow
            .steps
            .get(session.cursor.position)
            .cloned()
            .ok_or_else(|| RunnerError::FlowNotFound {
                flow_id: flow_id.to_string(),
            })?;

        let outcome = match step {
            FlowStep::Adapter(call) => {
                self.execute_adapter_step(&flow, &mut session, call, tenant)
                    .await?
            }
            FlowStep::AwaitInput { reason } => {
                session.waiting = Some(WaitState {
                    reason: reason.clone(),
                    recorded_at: SystemTime::now(),
                });
                session.last_outcome = Some(json!({
                    "status": "pending",
                    "reason": reason,
                }));
                session.last_outcome.clone().unwrap()
            }
            FlowStep::Complete { outcome } => {
                session.cursor.position = flow.steps.len();
                session.last_outcome = Some(json!({
                    "status": "done",
                    "result": outcome,
                }));
                session.last_outcome.clone().unwrap()
            }
        };

        self.host
            .state
            .set_json(&session.key, session.state.clone())
            .await?;

        if is_new {
            session_host.put(session).await?;
        } else if !session_host.update_cas(session, expected_revision).await? {
            return Err(RunnerError::Session {
                reason: "compare-and-swap failure".into(),
            });
        }

        Ok(outcome)
    }

    fn ensure_policy_budget(&self, flow: &FlowDefinition) -> GResult<()> {
        if flow.steps.len() > self.policy.max_egress_adapters {
            return Err(policy_violation(format!(
                "flow has {} steps exceeding budget {}",
                flow.steps.len(),
                self.policy.max_egress_adapters
            )));
        }
        Ok(())
    }

    async fn execute_adapter_step(
        &self,
        flow: &FlowDefinition,
        session: &mut SessionSnapshot,
        call: AdapterCall,
        tenant: &TenantCtx,
    ) -> GResult<Value> {
        let adapter =
            self.adapters
                .get(&call.adapter)
                .ok_or_else(|| RunnerError::AdapterMissing {
                    adapter: call.adapter.clone(),
                })?;

        let payload_bytes =
            serde_json::to_vec(&call.payload).map_err(|err| RunnerError::Serialization {
                reason: err.to_string(),
            })?;

        if payload_bytes.len() > self.policy.max_payload_bytes {
            return Err(policy_violation(format!(
                "payload exceeds max size {} bytes",
                self.policy.max_payload_bytes
            )));
        }

        let seq = session.cursor.outbox_seq;
        let payload_hash = Self::stable_hash(seq, &payload_bytes);
        let key = OutboxKey::new(seq, payload_hash.clone());

        let adapter_id = call.adapter.clone();
        let operation_id = call.operation.clone();
        let span = SpanContext {
            trace_id: tenant.trace_id.clone(),
            span_id: Some(format!("{}:{}", flow.summary.id, seq)),
        };

        if let Some(entry) = session.outbox.get(&key) {
            self.host
                .telemetry
                .emit(
                    &span,
                    &[
                        ("adapter", adapter_id.as_str()),
                        ("operation", operation_id.as_str()),
                        ("dedup", "hit"),
                    ],
                )
                .await?;
            session.cursor.position += 1;
            session.cursor.outbox_seq = session.cursor.outbox_seq.saturating_add(1);
            session.waiting = None;
            session.last_outcome = Some(json!({
                "status": "done",
                "response": entry.response.clone(),
            }));
            return Ok(session.last_outcome.clone().unwrap());
        }

        self.host
            .telemetry
            .emit(
                &span,
                &[
                    ("adapter", adapter_id.as_str()),
                    ("operation", operation_id.as_str()),
                    ("phase", "start"),
                ],
            )
            .await?;

        let adapter_clone = adapter.clone();
        let call_clone = call.clone();
        let response = retry_with_jitter(&self.policy.retry, || {
            let adapter = adapter_clone.clone();
            let call = call_clone.clone();
            async move { adapter.call(&call).await }
        })
        .await?;

        session.cursor.position += 1;
        session.cursor.outbox_seq = session.cursor.outbox_seq.saturating_add(1);
        session.waiting = None;
        session.outbox.insert(
            key,
            SessionOutboxEntry {
                seq,
                hash: payload_hash,
                response: response.clone(),
            },
        );
        Self::update_state_adapter(session, &call, &response);
        session.last_outcome = Some(json!({
            "status": "done",
            "response": response.clone(),
        }));

        self.host
            .telemetry
            .emit(
                &span,
                &[
                    ("adapter", adapter_id.as_str()),
                    ("operation", operation_id.as_str()),
                    ("phase", "finish"),
                ],
            )
            .await?;

        Ok(session.last_outcome.clone().unwrap())
    }

    fn update_state_input(session: &mut SessionSnapshot, input: Value) {
        if !matches!(session.state, Value::Object(_)) {
            session.state = Value::Object(Map::new());
        }
        if let Value::Object(map) = &mut session.state {
            map.insert("last_input".to_string(), input);
        }
    }

    fn update_state_adapter(session: &mut SessionSnapshot, call: &AdapterCall, response: &Value) {
        if !matches!(session.state, Value::Object(_)) {
            session.state = Value::Object(Map::new());
        }
        if let Value::Object(map) = &mut session.state {
            map.insert(
                "last_adapter".to_string(),
                Value::String(call.adapter.clone()),
            );
            map.insert(
                "last_operation".to_string(),
                Value::String(call.operation.clone()),
            );
            map.insert("last_response".to_string(), response.clone());
        }
    }

    fn stable_hash(seq: u64, payload: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(seq.to_be_bytes());
        hasher.update(payload);
        hex::encode(hasher.finalize())
    }

    fn generate_session_id() -> String {
        let mut rng = rng();
        let value: u128 = rng.random();
        format!("sess-{value:032x}")
    }
}
