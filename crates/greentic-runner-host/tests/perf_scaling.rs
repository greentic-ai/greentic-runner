use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use greentic_runner_host::routing::{RoutingConfig, TenantResolver, TenantRouting};

const ROUTING_ITERS: usize = 1_500;

fn sample_routing_workload(routing: &TenantRouting) {
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

fn run_scaling_workload(
    threads: usize,
    iterations_per_thread: usize,
    work: impl Fn() + Send + Sync + 'static,
) -> Duration {
    let start = Instant::now();
    let barrier = Arc::new(std::sync::Barrier::new(threads));
    let work = Arc::new(work);
    let handles = (0..threads)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let work = Arc::clone(&work);
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..iterations_per_thread {
                    work();
                }
            })
        })
        .collect::<Vec<_>>();

    for handle in handles {
        handle.join().expect("worker thread");
    }
    start.elapsed()
}

fn nanos_per_op(duration: Duration, threads: usize, iterations_per_thread: usize) -> f64 {
    duration.as_nanos() as f64 / (threads * iterations_per_thread) as f64
}

#[test]
fn jwt_routing_scaling_stays_reasonable() {
    let routing = Arc::new(TenantRouting::new(RoutingConfig {
        resolver: TenantResolver::Jwt {
            header: axum::http::header::AUTHORIZATION,
            claim: "tenant".into(),
        },
        default_tenant: "demo".into(),
    }));

    let t1 = run_scaling_workload(1, ROUTING_ITERS, {
        let routing = Arc::clone(&routing);
        move || sample_routing_workload(&routing)
    });
    let t4 = run_scaling_workload(4, ROUTING_ITERS, {
        let routing = Arc::clone(&routing);
        move || sample_routing_workload(&routing)
    });
    let t8 = run_scaling_workload(8, ROUTING_ITERS, {
        let routing = Arc::clone(&routing);
        move || sample_routing_workload(&routing)
    });

    let p1 = nanos_per_op(t1, 1, ROUTING_ITERS);
    let p4 = nanos_per_op(t4, 4, ROUTING_ITERS);
    let p8 = nanos_per_op(t8, 8, ROUTING_ITERS);

    assert!(
        p4 <= p1 * 2.5,
        "jwt routing scaled poorly at 4 threads: t1={:?}, t4={:?}, p1={:.1}ns/op, p4={:.1}ns/op",
        t1,
        t4,
        p1,
        p4
    );
    assert!(
        p8 <= p1 * 4.0,
        "jwt routing scaled poorly at 8 threads: t1={:?}, t8={:?}, p1={:.1}ns/op, p8={:.1}ns/op",
        t1,
        t8,
        p1,
        p8
    );
}
