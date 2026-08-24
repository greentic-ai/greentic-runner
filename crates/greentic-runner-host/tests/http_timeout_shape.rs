//! Characterizes how `wasmtime-wasi-http` surfaces a connect timeout, at the
//! exact seam `HttpTimeoutHooks::send_request` (Task 2) will delegate to.
//! This is load-bearing for the rest of this plan: it is what confirmed the
//! timeout is guest-visible (an inner `wasi:http` error-code the component
//! sees), not a host-level `Err`, which is why no new detection code was
//! added to `runner/engine.rs` — the existing `component_error` /
//! `has_error_route` path (Phase 1-3) already routes whatever shape
//! `component-http` reshapes this into.

use std::time::Duration;

use http_body_util::BodyExt;
use wasmtime_wasi_http::p2::default_send_request;
use wasmtime_wasi_http::p2::types::{self, OutgoingRequestConfig};

#[tokio::test]
async fn connect_timeout_is_a_guest_visible_error_code_not_a_host_err() {
    // RFC 5737 TEST-NET-1: reserved for documentation, routers silently drop
    // packets sent to it, so the TCP connect attempt hangs until our
    // configured timeout fires rather than getting an immediate
    // ConnectionRefused (which would prove nothing about timeout behavior).
    let req = hyper::Request::builder()
        .uri("http://192.0.2.1:9")
        .body(
            http_body_util::Empty::<bytes::Bytes>::new()
                .map_err(|_: std::convert::Infallible| unreachable!())
                .boxed_unsync(),
        )
        .unwrap();
    let config = OutgoingRequestConfig {
        use_tls: false,
        connect_timeout: Duration::from_millis(200),
        first_byte_timeout: Duration::from_secs(5),
        between_bytes_timeout: Duration::from_secs(5),
    };

    let resp = default_send_request(req, config);
    let types::HostFutureIncomingResponse::Pending(handle) = resp else {
        panic!("expected Pending — default_send_request always spawns");
    };

    // `FutureIncomingResponseHandle` is `AbortOnDropJoinHandle<wasmtime::Result<
    // Result<IncomingResponse, types::ErrorCode>>>`, and `AbortOnDropJoinHandle<T>`
    // implements `Future<Output = T>` directly, so it can be awaited here with
    // no wasi:io/poll Component Model resource-table machinery involved.
    let outer = handle.await;

    let Ok(inner) = outer else {
        panic!(
            "found a host-level Err — this means the spec's branch 1 is the \
             real one, not branch 2. Stop: this test's finding contradicts \
             the rest of this plan and Tasks 2-3 need re-scoping before \
             continuing. outer = {outer:?}"
        );
    };
    let Err(code) = inner else {
        panic!("expected a connect failure, got a real response: {inner:?}");
    };
    assert_eq!(
        format!("{code:?}"),
        "ErrorCode::ConnectionTimeout",
        "the specific error variant may legitimately differ (e.g. under a \
         different OS/network stack), but it must still be the guest-visible \
         inner Err — this exact assertion is what this test exists to pin"
    );
}
