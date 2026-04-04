use std::hint::black_box;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use criterion::{Criterion, criterion_group, criterion_main};
use greentic_runner_host::routing::{RoutingConfig, TenantResolver, TenantRouting};
use runner_core::normalize_under_root;

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
    targets = bench_jwt_routing, bench_normalize_under_root
}
criterion_main!(benches);
