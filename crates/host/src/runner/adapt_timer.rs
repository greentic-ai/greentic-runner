use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cron::Schedule;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::sleep;

use super::{ServerState, engine::FlowContext};

pub fn spawn_timers(state: Arc<ServerState>) -> Result<Vec<JoinHandle<()>>> {
    let mut handles = Vec::new();

    for timer in state.config.timers.clone() {
        let cron_expr = timer.cron.clone();
        let schedule = Schedule::from_str(&cron_expr)
            .with_context(|| format!("invalid cron expression for {}", timer.schedule_id()))?;
        let flow_id = timer.flow_id.clone();
        let schedule_id = timer.schedule_id().to_string();
        let shared_state = Arc::clone(&state);
        let tenant = shared_state.config.tenant.clone();

        let handle = tokio::spawn(async move {
            let engine = Arc::clone(&shared_state.engine);

            tracing::info!(
                flow_id = %flow_id,
                schedule_id = %schedule_id,
                cron = %cron_expr,
                "registered timer schedule"
            );
            for next in schedule.upcoming(Utc) {
                if let Some(wait) = duration_until(next) {
                    sleep(wait).await;
                } else {
                    continue;
                }
                let payload = json!({
                    "now": next.to_rfc3339(),
                    "schedule_id": schedule_id.clone(),
                });
                tracing::info!(
                    flow_id = %flow_id,
                    schedule_id = %schedule_id,
                    scheduled_for = %next,
                    "triggering timer flow"
                );
                match engine
                    .execute(
                        FlowContext {
                            tenant: &tenant,
                            flow_id: &flow_id,
                            node_id: None,
                            tool: None,
                            action: Some("timer"),
                            session_id: Some(schedule_id.as_str()),
                            provider_id: None,
                            retry_config: shared_state.config.mcp_retry_config().into(),
                            observer: None,
                            mocks: None,
                        },
                        payload,
                    )
                    .await
                {
                    Ok(output) => {
                        tracing::info!(
                            flow_id = %flow_id,
                            schedule_id = %schedule_id,
                            now = %next,
                            response = %output,
                            "timer flow completed"
                        );
                    }
                    Err(err) => {
                        let chain = err.chain().map(|e| e.to_string()).collect::<Vec<_>>();
                        tracing::error!(
                            flow_id = %flow_id,
                            schedule_id = %schedule_id,
                            error.cause_chain = ?chain,
                            "timer flow execution failed"
                        );
                    }
                }
            }
            tracing::info!(
                flow_id = %flow_id,
                schedule_id = %schedule_id,
                "timer schedule completed"
            );
        });

        handles.push(handle);
    }

    Ok(handles)
}

fn duration_until(next: DateTime<Utc>) -> Option<Duration> {
    let now = Utc::now();
    let duration = next - now;
    if duration.num_milliseconds() <= 0 {
        return Some(Duration::from_secs(0));
    }
    duration.to_std().ok()
}
