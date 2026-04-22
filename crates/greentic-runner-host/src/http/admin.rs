use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;
use time::format_description::well_known::Rfc3339;

use crate::http::auth::AdminGuard;
use crate::runner::ServerState;

pub async fn status(AdminGuard: AdminGuard, State(state): State<ServerState>) -> impl IntoResponse {
    let snapshot = state.active.snapshot();
    let tenants = snapshot
        .iter()
        .map(|(tenant, runtime)| {
            let pack = runtime.pack();
            let metadata = pack.metadata();
            let required_secrets = runtime.required_secrets();
            let missing_secrets = runtime.missing_secrets();
            let overlays = runtime
                .overlays()
                .into_iter()
                .zip(runtime.overlay_digests())
                .map(|(overlay, digest)| {
                    let meta = overlay.metadata();
                    json!({
                        "pack_id": meta.pack_id,
                        "version": meta.version,
                        "digest": digest,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "tenant": tenant,
                "pack_id": metadata.pack_id,
                "version": metadata.version,
                "digest": runtime.digest(),
                "overlays": overlays,
                "required_secrets": required_secrets,
                "missing_secrets": missing_secrets,
            })
        })
        .collect::<Vec<_>>();

    let health = state.health.snapshot();
    let last_reload = health.last_reload.and_then(|ts| ts.format(&Rfc3339).ok());

    Json(json!({
        "tenants": tenants,
        "active": snapshot.len(),
        "last_reload": last_reload,
        "last_error": health.last_error,
    }))
}

pub async fn reload(AdminGuard: AdminGuard, State(state): State<ServerState>) -> impl IntoResponse {
    if let Some(handle) = &state.reload {
        match handle.trigger().await {
            Ok(()) => {
                tracing::info!("pack.reload.requested");
                (
                    StatusCode::ACCEPTED,
                    Json(json!({ "status": "reload requested" })),
                )
            }
            Err(err) => {
                tracing::warn!(error = %err, "reload trigger failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": err.to_string() })),
                )
            }
        }
    } else {
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": "reload handle unavailable" })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::auth::AdminAuth;
    use crate::http::health::HealthState;
    use crate::routing::{RoutingConfig, TenantRouting};
    use crate::runtime::ActivePacks;
    use axum::body::to_bytes;
    use axum::response::Response;
    use std::sync::Arc;

    fn state() -> ServerState {
        ServerState {
            active: Arc::new(ActivePacks::new()),
            routing: TenantRouting::new(RoutingConfig::default()),
            health: Arc::new(HealthState::new()),
            reload: None,
            admin: AdminAuth::default(),
        }
    }

    async fn json_body(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        serde_json::from_slice(&body).expect("json body")
    }

    #[tokio::test]
    async fn status_reports_empty_runtime_snapshot() {
        let state = state();
        state.health.record_reload_success();

        let response = status(AdminGuard, State(state)).await.into_response();
        let body = json_body(response).await;

        assert_eq!(body["active"], 0);
        assert_eq!(body["tenants"], serde_json::Value::Array(Vec::new()));
        assert!(body["last_reload"].is_string());
        assert!(body["last_error"].is_null());
    }

    #[tokio::test]
    async fn reload_without_handle_reports_not_implemented() {
        let response = reload(AdminGuard, State(state())).await.into_response();

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = json_body(response).await;
        assert_eq!(body["error"], "reload handle unavailable");
    }
}
