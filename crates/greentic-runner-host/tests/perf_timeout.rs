use std::hint::black_box;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use greentic_runner_host::routing::{RoutingConfig, TenantResolver, TenantRouting};
use runner_core::normalize_under_root;

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
