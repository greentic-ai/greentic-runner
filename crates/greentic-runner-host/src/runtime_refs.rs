//! Per-pack `runtime://` URI resolution for the C5 runtime-refs channel.
//!
//! `pack-config.v1.runtime_refs` binds opaque keys to `runtime://<env>/...`
//! URIs whose values live in the env-pack-emitted `runtime.json`
//! ([`EnvironmentRuntime`]). The producer (greentic-start) owns the
//! `EnvironmentRuntime` snapshot + hot-reload watcher and exposes lookups
//! through [`RuntimeRefResolver`].
//!
//! Runner-host stays unaware of URI shapes: the per-pack `refs` map carries
//! opaque URI strings, and the resolver opaquely returns the current value.
//! This shape preserves hot-reload — every host-import call re-enters the
//! resolver, which reads from start's `ArcSwap` snapshot.
//!
//! [`EnvironmentRuntime`]: greentic-deploy-spec::EnvironmentRuntime

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;
use thiserror::Error;

/// Errors a [`RuntimeRefResolver`] can return. Mapped onto
/// `greentic:runtime-config@1.0.0::ConfigError` by the host import.
#[derive(Debug, Error)]
pub enum RuntimeRefResolverError {
    /// The URI did not resolve in the current `EnvironmentRuntime` snapshot.
    #[error("runtime-ref not found")]
    NotFound,
    /// URI shape was rejected by the resolver (parse / env mismatch).
    #[error("runtime-ref invalid: {0}")]
    Invalid(String),
    /// Resolver was unable to complete the lookup (snapshot read failure,
    /// store error, etc.). Treated by the host import as `Internal`.
    #[error("runtime-ref internal error: {0}")]
    Internal(String),
}

/// Trait implemented by greentic-start to resolve `runtime://` URIs against
/// the live [`EnvironmentRuntime`] snapshot. Implementations MUST be cheap
/// to call (they are invoked on every `greentic:runtime-config/get`).
///
/// [`EnvironmentRuntime`]: greentic-deploy-spec::EnvironmentRuntime
pub trait RuntimeRefResolver: Debug + Send + Sync {
    /// Resolve a single `runtime://` URI. Returns `Ok(None)` when the URI
    /// parses cleanly but is not bound in the current snapshot — the host
    /// import maps this to `ConfigError::NotFound`.
    fn resolve(&self, runtime_ref: &str) -> Result<Option<Value>, RuntimeRefResolverError>;
}

/// Per-pack runtime-refs injection: the `key → URI` map from
/// `pack-config.v1.runtime_refs` plus the env-wide resolver. The resolver
/// is shared across every pack in the env (one `ArcSwap` snapshot owner);
/// the `refs` map is per-pack.
#[derive(Clone)]
pub struct RuntimeRefsInjection {
    /// `key → "runtime://<env>/discovered/<path>"`. Keys are what the WASM
    /// component asks for; URIs are opaque to runner-host and forwarded to
    /// the resolver verbatim.
    pub refs: Arc<BTreeMap<String, String>>,
    /// Env-shared resolver. Reads start's `EnvironmentRuntime` snapshot on
    /// every call so hot-reloads land immediately.
    pub resolver: Arc<dyn RuntimeRefResolver>,
}

impl Debug for RuntimeRefsInjection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeRefsInjection")
            .field("refs", &self.refs)
            .field("resolver", &self.resolver)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test resolver that returns a fixed value for one URI.
    #[derive(Debug)]
    struct FixedResolver {
        uri: String,
        value: Value,
    }

    impl RuntimeRefResolver for FixedResolver {
        fn resolve(&self, runtime_ref: &str) -> Result<Option<Value>, RuntimeRefResolverError> {
            if runtime_ref == self.uri {
                Ok(Some(self.value.clone()))
            } else {
                Ok(None)
            }
        }
    }

    #[test]
    fn resolver_returns_value_for_bound_uri() {
        let resolver = FixedResolver {
            uri: "runtime://local/discovered/alb_dns".into(),
            value: Value::String("alb.example.com".into()),
        };
        let value = resolver
            .resolve("runtime://local/discovered/alb_dns")
            .unwrap();
        assert_eq!(value, Some(Value::String("alb.example.com".into())));
    }

    #[test]
    fn resolver_returns_none_for_unbound_uri() {
        let resolver = FixedResolver {
            uri: "runtime://local/discovered/alb_dns".into(),
            value: Value::String("alb.example.com".into()),
        };
        assert_eq!(
            resolver
                .resolve("runtime://local/discovered/other")
                .unwrap(),
            None,
        );
    }
}
