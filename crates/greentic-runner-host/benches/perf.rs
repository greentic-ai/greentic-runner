use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
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

fn sample_payload_input() -> (
    ProviderIds,
    String,
    Vec<String>,
    chrono::DateTime<Utc>,
    serde_json::Value,
    serde_json::Value,
) {
    let provider_ids = sample_provider_ids();
    let session_key = canonical_session_key("demo", "slack", &provider_ids);
    let scopes = vec![
        "chat".to_string(),
        "attachments".to_string(),
        "buttons".to_string(),
    ];
    let raw = json!({
        "type": "event_callback",
        "event": {
            "text": "hello",
            "blocks": [{
                "elements": [{
                    "type": "button",
                    "action_id": "approve",
                    "text": { "text": "Approve" },
                    "value": "approve"
                }]
            }]
        }
    });
    let channel_data = json!({
        "type": "message",
        "service_url": "https://example.invalid",
    });
    (
        provider_ids,
        session_key,
        scopes,
        Utc::now(),
        raw,
        channel_data,
    )
}

fn sample_jwt_request() -> axum::http::request::Parts {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"tenant":"jwt-tenant","scope":"perf"}"#);
    axum::http::Request::builder()
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer header.{payload}.signature"),
        )
        .body(())
        .expect("request")
        .into_parts()
        .0
}

fn bench_canonical_payload(c: &mut Criterion) {
    let (provider_ids, session_key, scopes, timestamp, raw, channel_data) = sample_payload_input();
    c.bench_function("host/build_canonical_payload", |b| {
        b.iter(|| {
            black_box(build_canonical_payload(
                black_box("demo"),
                black_box("slack"),
                black_box(&provider_ids),
                black_box(session_key.clone()),
                black_box(&scopes),
                black_box(timestamp),
                black_box(Some("en".to_string())),
                black_box(Some("hello from perf bench".to_string())),
                black_box(vec![json!({
                    "type": "image",
                    "url": "https://example.invalid/image.png",
                    "size": 1024
                })]),
                black_box(vec![json!({
                    "id": "approve",
                    "title": "Approve",
                    "payload": "approve"
                })]),
                black_box(empty_entities()),
                black_box(default_metadata()),
                black_box(channel_data.clone()),
                black_box(raw.clone()),
            ));
        });
    });
}

fn bench_jwt_routing(c: &mut Criterion) {
    let routing = TenantRouting::new(RoutingConfig {
        resolver: TenantResolver::Jwt {
            header: axum::http::header::AUTHORIZATION,
            claim: "tenant".into(),
        },
        default_tenant: "demo".into(),
    });
    c.bench_function("host/jwt_routing_resolve", |b| {
        b.iter(|| {
            let parts = sample_jwt_request();
            black_box(routing.resolve(black_box(&parts)).expect("tenant"));
        });
    });
}

fn bench_normalize_under_root(c: &mut Criterion) {
    let cwd = std::env::current_dir().expect("cwd");
    c.bench_function("runner_core/normalize_under_root", |b| {
        b.iter(|| {
            black_box(
                normalize_under_root(black_box(&cwd), black_box(Path::new("Cargo.toml")))
                    .expect("normalize"),
            );
        });
    });
}

fn perf_benches() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(150))
        .measurement_time(Duration::from_millis(400))
}

criterion_group! {
    name = benches;
    config = perf_benches();
    targets = bench_canonical_payload, bench_jwt_routing, bench_normalize_under_root
}
criterion_main!(benches);
