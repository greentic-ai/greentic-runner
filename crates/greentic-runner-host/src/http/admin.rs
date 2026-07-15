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

/// Serialise a capability registry's offerings into the `/admin/capabilities`
/// response body.
///
/// CROSS-REPO CONTRACT. `greentic-designer-admin` parses this exact shape to
/// preflight guardrail policies (see that repo's
/// `docs/superpowers/specs/2026-07-15-guardrail-policy-preflight-design.md`).
/// Renaming a key does not fail its build — the preflight silently degrades to
/// "no capabilities offered". Change this shape only alongside that consumer.
///
/// Sorted by `(cap_id, extension_id)`: `offerings()` walks a `HashMap`, so the
/// order is otherwise arbitrary between calls.
#[cfg(feature = "agentic-worker")]
fn offerings_to_json(registry: &greentic_ext_runtime::CapabilityRegistry) -> serde_json::Value {
    let mut offerings: Vec<&greentic_ext_runtime::OfferedBinding> = registry.offerings().collect();
    offerings.sort_by(|a, b| {
        a.cap_id
            .to_string()
            .cmp(&b.cap_id.to_string())
            .then_with(|| a.extension_id.cmp(&b.extension_id))
    });
    let caps: Vec<serde_json::Value> = offerings
        .into_iter()
        .map(|offering| {
            json!({
                "extension_id": offering.extension_id,
                "cap_id": offering.cap_id.to_string(),
                "version": offering.version.to_string(),
                "kind": offering.kind,
            })
        })
        .collect();
    json!({ "capabilities": caps })
}

/// `GET /admin/capabilities` — report the capabilities installed on this runner.
///
/// Consumed by `greentic-designer-admin` to preflight a mandatory guardrail
/// policy before saving it: a policy naming a cap absent from this list will
/// fail closed at runtime (`greentic-aw-runtime` `loop.rs`), blocking every
/// agent turn in scope.
#[cfg(feature = "agentic-worker")]
pub async fn capabilities(_: AdminGuard, State(state): State<ServerState>) -> impl IntoResponse {
    let body = match &state.ext_runtime {
        Some(runtime) => offerings_to_json(&runtime.capability_registry()),
        None => json!({ "capabilities": [] }),
    };
    (StatusCode::OK, Json(body))
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
        let host = crate::host::RunnerHost::for_test();
        ServerState {
            active: Arc::new(ActivePacks::new()),
            routing: TenantRouting::new(RoutingConfig::default()),
            health: Arc::new(HealthState::new()),
            reload: None,
            admin: AdminAuth::default(),
            #[cfg(feature = "agentic-worker")]
            stream_observers: host.stream_observers(),
            #[cfg(feature = "agentic-worker")]
            ext_runtime: None,
            host,
            sql: crate::sql::SqlGateway::new(std::collections::HashMap::new(), String::new()),
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

    #[cfg(feature = "agentic-worker")]
    #[test]
    fn offerings_to_json_matches_the_admin_contract() {
        use greentic_ext_runtime::{CapabilityRegistry, OfferedBinding};
        use greentic_extension_sdk_contract::ExtensionKind;

        let mut registry = CapabilityRegistry::new();
        registry.add_offering(OfferedBinding {
            extension_id: "greentic.guardrail-pii".to_string(),
            cap_id: "greentic:guardrail/pii".parse().unwrap(),
            version: "0.1.0".parse().unwrap(),
            kind: ExtensionKind::Design,
            export_path: String::new(),
        });

        assert_eq!(
            offerings_to_json(&registry),
            serde_json::json!({
                "capabilities": [{
                    "extension_id": "greentic.guardrail-pii",
                    "cap_id": "greentic:guardrail/pii",
                    "version": "0.1.0",
                    "kind": "DesignExtension"
                }]
            })
        );
    }

    #[cfg(feature = "agentic-worker")]
    #[test]
    fn offerings_to_json_is_ordered_and_handles_empty() {
        use greentic_ext_runtime::{CapabilityRegistry, OfferedBinding};
        use greentic_extension_sdk_contract::ExtensionKind;

        assert_eq!(
            offerings_to_json(&CapabilityRegistry::new()),
            serde_json::json!({ "capabilities": [] })
        );

        // `offerings()` walks a HashMap, so ordering is not inherent. Two caps
        // inserted in reverse order must still come out sorted, or the admin's
        // response and these assertions become flaky.
        let mut registry = CapabilityRegistry::new();
        for cap in ["greentic:guardrail/secrets", "greentic:guardrail/injection"] {
            registry.add_offering(OfferedBinding {
                extension_id: "ext".to_string(),
                cap_id: cap.parse().unwrap(),
                version: "1.0.0".parse().unwrap(),
                kind: ExtensionKind::Design,
                export_path: String::new(),
            });
        }
        let body = offerings_to_json(&registry);
        let caps = body["capabilities"].as_array().unwrap();
        assert_eq!(caps[0]["cap_id"], "greentic:guardrail/injection");
        assert_eq!(caps[1]["cap_id"], "greentic:guardrail/secrets");
    }
}
