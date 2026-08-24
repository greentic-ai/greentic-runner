//! Host-configured ceiling on outbound `wasi:http` request timeouts.
//!
//! Without this, a component's outbound HTTP call is bounded only by
//! `wasmtime-wasi-http`'s own library default — 600 seconds per phase
//! (connect / first byte / between bytes) — so a hung remote server hangs
//! the whole flow for up to ten minutes. [`HttpTimeoutHooks`] clamps each
//! phase down to [`ceiling()`] before delegating to the library's own
//! [`default_send_request`], so a component's own shorter, explicitly-set
//! timeout is never raised — only an absent or longer one is lowered.

use std::time::Duration;

use wasmtime_wasi_http::p2::body::HyperOutgoingBody;
use wasmtime_wasi_http::p2::types::OutgoingRequestConfig;
use wasmtime_wasi_http::p2::{HttpResult, WasiHttpHooks, default_send_request, types};

/// Ceiling applied when no operator override is set. Far below the
/// library's own 600s-per-phase default — generous enough for a normal API
/// call, short enough that "the flow hangs forever" becomes "the flow fails
/// within half a minute."
pub(crate) const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Operator override for [`DEFAULT_HTTP_TIMEOUT`], in whole seconds.
pub(crate) const HTTP_TIMEOUT_ENV: &str = "GREENTIC_HTTP_OUTBOUND_TIMEOUT_SECS";

/// Resolve the ceiling from an already-read env value. Pure so it is
/// testable without mutating the process environment. Anything unusable —
/// absent, unparseable, or zero — falls back to the default rather than
/// failing: a malformed knob must not make every outbound HTTP call
/// instantly fail, which is the opposite of what this feature exists to fix.
pub(crate) fn ceiling_from(raw: Option<&str>) -> Duration {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_HTTP_TIMEOUT)
}

/// The outbound HTTP timeout ceiling for this process.
pub(crate) fn ceiling() -> Duration {
    ceiling_from(std::env::var(HTTP_TIMEOUT_ENV).ok().as_deref())
}

/// [`WasiHttpHooks`] implementation that clamps every outbound request's
/// three timeout phases to a host ceiling before delegating to
/// [`default_send_request`] for everything else — the real async-level
/// `tokio::time::timeout` enforcement lives there, unchanged.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HttpTimeoutHooks {
    ceiling: Duration,
}

impl HttpTimeoutHooks {
    /// Build from the process environment (reads [`HTTP_TIMEOUT_ENV`]).
    pub(crate) fn from_env() -> Self {
        Self { ceiling: ceiling() }
    }

    #[cfg(test)]
    fn with_ceiling(ceiling: Duration) -> Self {
        Self { ceiling }
    }
}

/// Clamp all three timeout phases to the given ceiling.
fn clamp_config(config: OutgoingRequestConfig, ceiling: Duration) -> OutgoingRequestConfig {
    OutgoingRequestConfig {
        use_tls: config.use_tls,
        connect_timeout: config.connect_timeout.min(ceiling),
        first_byte_timeout: config.first_byte_timeout.min(ceiling),
        between_bytes_timeout: config.between_bytes_timeout.min(ceiling),
    }
}

impl WasiHttpHooks for HttpTimeoutHooks {
    fn send_request(
        &mut self,
        request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> HttpResult<types::HostFutureIncomingResponse> {
        let clamped = clamp_config(config, self.ceiling);
        Ok(default_send_request(request, clamped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_from_missing_env_falls_back_to_default() {
        assert_eq!(ceiling_from(None), DEFAULT_HTTP_TIMEOUT);
    }

    #[test]
    fn ceiling_from_unparseable_env_falls_back_to_default() {
        assert_eq!(ceiling_from(Some("not-a-number")), DEFAULT_HTTP_TIMEOUT);
    }

    #[test]
    fn ceiling_from_zero_falls_back_to_default() {
        assert_eq!(ceiling_from(Some("0")), DEFAULT_HTTP_TIMEOUT);
    }

    #[test]
    fn ceiling_from_valid_env_is_honored() {
        assert_eq!(ceiling_from(Some("45")), Duration::from_secs(45));
    }

    #[test]
    fn ceiling_from_env_trims_whitespace() {
        assert_eq!(ceiling_from(Some("  45  ")), Duration::from_secs(45));
    }

    #[test]
    fn send_request_lowers_a_longer_or_absent_field() {
        let ceiling = Duration::from_secs(10);
        // The library's own 600s-per-phase default, unmodified — the "absent
        // guest override" case.
        let config = OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: Duration::from_secs(600),
            first_byte_timeout: Duration::from_secs(600),
            between_bytes_timeout: Duration::from_secs(600),
        };
        let clamped = clamp_config(config, ceiling);
        assert_eq!(clamped.connect_timeout, Duration::from_secs(10));
        assert_eq!(clamped.first_byte_timeout, Duration::from_secs(10));
        assert_eq!(clamped.between_bytes_timeout, Duration::from_secs(10));
    }

    #[test]
    fn a_guest_supplied_shorter_value_is_never_raised() {
        let ceiling = Duration::from_secs(30);
        let guest_value = Duration::from_secs(3);
        let config = OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: guest_value,
            first_byte_timeout: guest_value,
            between_bytes_timeout: guest_value,
        };
        let clamped = clamp_config(config, ceiling);
        assert_eq!(
            clamped.connect_timeout, guest_value,
            "a 3s guest value must survive unchanged against a 30s ceiling"
        );
        assert_eq!(clamped.first_byte_timeout, guest_value);
        assert_eq!(clamped.between_bytes_timeout, guest_value);
    }

    #[test]
    fn a_guest_supplied_longer_value_is_lowered() {
        let ceiling = Duration::from_secs(30);
        let guest_value = Duration::from_secs(600);
        let config = OutgoingRequestConfig {
            use_tls: false,
            connect_timeout: guest_value,
            first_byte_timeout: guest_value,
            between_bytes_timeout: guest_value,
        };
        let clamped = clamp_config(config, ceiling);
        assert_eq!(
            clamped.connect_timeout, ceiling,
            "a 600s guest value must be lowered to the 30s ceiling"
        );
        assert_eq!(clamped.first_byte_timeout, ceiling);
        assert_eq!(clamped.between_bytes_timeout, ceiling);
    }

    fn test_request() -> hyper::Request<HyperOutgoingBody> {
        use http_body_util::BodyExt;
        hyper::Request::builder()
            .uri("http://192.0.2.1:9")
            .body(
                http_body_util::Empty::<bytes::Bytes>::new()
                    .map_err(|_: std::convert::Infallible| unreachable!())
                    .boxed_unsync(),
            )
            .unwrap()
    }
}
