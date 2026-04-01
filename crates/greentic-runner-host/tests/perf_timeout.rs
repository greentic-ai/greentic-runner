use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::Utc;
use greentic_runner_host::ingress::{
    ProviderIds, build_canonical_payload, canonical_session_key, default_metadata, empty_entities,
};
use greentic_runner_host::routing::{RoutingConfig, TenantResolver, TenantRouting};
use runner_core::normalize_under_root;
use serde_json::json;

fn sample_provider_ids() -> ProviderIds {
    ProviderIds {
        workspace_id: Some("T123".into()),
        conversation_id: Some("conv-123".into()),
        thread_id: Some("thread-123".into()),
        channel_id: Some("channel-123".into()),
        user_id: Some("user-123".into()),
        message_id: Some("message-123".into()),
        event_id: Some("event-123".into()),
        ..ProviderIds::default()
    }
}

#[test]
fn perf_guards_finish_quickly() {
    let start = Instant::now();
    let routing = TenantRouting::new(RoutingConfig {
        resolver: TenantResolver::Jwt {
            header: axum::http::header::AUTHORIZATION,
            claim: "tenant".into(),
        },
        default_tenant: "demo".into(),
    });
    let root = std::env::current_dir().expect("cwd");

    for _ in 0..5_000 {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"tenant":"jwt-tenant","scope":"perf"}"#);
        let (parts, _) = axum::http::Request::builder()
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer header.{payload}.signature"),
            )
            .body(())
            .expect("request")
            .into_parts();
        black_box(routing.resolve(&parts).expect("tenant"));
    }

    let provider_ids = sample_provider_ids();
    for _ in 0..2_000 {
        black_box(build_canonical_payload(
            "demo",
            "slack",
            &provider_ids,
            canonical_session_key("demo", "slack", &provider_ids),
            &["chat".to_string(), "attachments".to_string()],
            Utc::now(),
            Some("en".into()),
            Some("hello".into()),
            vec![json!({
                "type": "image",
                "url": "https://example.invalid/image.png",
                "size": 1024
            })],
            Vec::new(),
            empty_entities(),
            default_metadata(),
            json!({"type": "message"}),
            json!({"raw": true}),
        ));
    }

    for _ in 0..5_000 {
        black_box(normalize_under_root(&root, Path::new("Cargo.toml")).expect("normalize"));
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "perf guard workload too slow: {:?}",
        elapsed
    );
}
