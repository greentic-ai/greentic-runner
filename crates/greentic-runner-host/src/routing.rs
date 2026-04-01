use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use axum::extract::{FromRef, FromRequestParts};
use axum::http::header::{AUTHORIZATION, HOST};
use axum::http::request::Parts;
use axum::http::{HeaderName, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::Value;
use serde_json::json;

use crate::runner::ServerState;
use crate::runtime::TenantRuntime;

#[derive(Clone)]
pub struct RoutingConfig {
    pub resolver: TenantResolver,
    pub default_tenant: String,
}

impl RoutingConfig {
    pub fn from_env() -> Self {
        Self::from_env_with_default("demo".into())
    }

    pub fn from_env_with_default(default_tenant: String) -> Self {
        let default_tenant = std::env::var("DEFAULT_TENANT").unwrap_or(default_tenant);
        let resolver = std::env::var("TENANT_RESOLVER")
            .map(|value| TenantResolver::from_str(&value, &default_tenant))
            .unwrap_or(Ok(TenantResolver::Env))
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "invalid TENANT_RESOLVER, falling back to env");
                TenantResolver::Env
            });
        Self {
            resolver,
            default_tenant,
        }
    }
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            resolver: TenantResolver::Env,
            default_tenant: "demo".into(),
        }
    }
}

#[derive(Clone)]
pub enum TenantResolver {
    Host,
    Header(HeaderName),
    Jwt { header: HeaderName, claim: String },
    Env,
}

impl TenantResolver {
    fn from_str(value: &str, _default: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "host" => Ok(Self::Host),
            "header" => Ok(Self::Header(HeaderName::from_static("x-greentic-tenant"))),
            "jwt" => Ok(Self::Jwt {
                header: AUTHORIZATION,
                claim: "tenant".into(),
            }),
            "env" => Ok(Self::Env),
            other => bail!("unsupported TENANT_RESOLVER `{other}`"),
        }
    }
}

#[derive(Clone)]
pub struct TenantRouting {
    resolver: TenantResolver,
    default_tenant: String,
}

impl TenantRouting {
    pub fn new(cfg: RoutingConfig) -> Self {
        Self {
            resolver: cfg.resolver,
            default_tenant: cfg.default_tenant,
        }
    }

    pub fn resolve(&self, parts: &Parts) -> Result<String> {
        match &self.resolver {
            TenantResolver::Env => Ok(self.default_tenant.clone()),
            TenantResolver::Host => {
                let host = parts
                    .headers
                    .get(HOST)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default();
                if host.is_empty() {
                    return Ok(self.default_tenant.clone());
                }
                Ok(host
                    .split('.')
                    .next()
                    .map(|segment| segment.to_string())
                    .filter(|segment| !segment.is_empty())
                    .unwrap_or_else(|| self.default_tenant.clone()))
            }
            TenantResolver::Header(name) => {
                let tenant = parts
                    .headers
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| !value.is_empty())
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| self.default_tenant.clone());
                Ok(tenant)
            }
            TenantResolver::Jwt { header, claim } => {
                let token = parts
                    .headers
                    .get(header)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .ok_or_else(|| anyhow!("authorization header missing"))?;
                let tenant = decode_jwt_claim(token, claim)
                    .unwrap_or_else(|err| {
                        tracing::warn!(error = %err, "failed to decode jwt claim");
                        None
                    })
                    .unwrap_or_else(|| self.default_tenant.clone());
                Ok(tenant)
            }
        }
    }
}

fn decode_jwt_claim(token: &str, claim: &str) -> Result<Option<String>> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("invalid jwt structure"))?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.as_bytes()).or_else(|_| {
        let padded = match payload.len() % 4 {
            2 => Some(format!("{payload}==")),
            3 => Some(format!("{payload}=")),
            _ => None,
        };
        if let Some(padded) = padded.as_deref() {
            URL_SAFE
                .decode(padded.as_bytes())
                .or_else(|_| STANDARD.decode(padded.as_bytes()))
        } else {
            URL_SAFE
                .decode(payload.as_bytes())
                .or_else(|_| STANDARD.decode(payload.as_bytes()))
        }
    })?;
    let value: Value = serde_json::from_slice(&bytes)?;
    Ok(value
        .get(claim)
        .and_then(|node| node.as_str())
        .map(|value| value.to_string()))
}

pub struct TenantRuntimeHandle {
    pub tenant: String,
    pub runtime: Arc<TenantRuntime>,
}

impl<S> FromRequestParts<S> for TenantRuntimeHandle
where
    ServerState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, axum::Json<Value>);

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let server_state = ServerState::from_ref(state);
        async move {
            let tenant = server_state.routing.resolve(parts).map_err(|err| {
                (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": err.to_string() })),
                )
            })?;
            let runtime = server_state.active.load(&tenant).ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({ "error": "tenant not loaded" })),
                )
            })?;
            Ok(Self { tenant, runtime })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn host_resolver_picks_subdomain() {
        let routing = TenantRouting::new(RoutingConfig {
            resolver: TenantResolver::Host,
            default_tenant: "demo".into(),
        });
        let (parts, _) = Request::builder()
            .uri("http://foo.example.com/webhook")
            .header(HOST, "foo.example.com")
            .body(())
            .unwrap()
            .into_parts();
        let tenant = routing.resolve(&parts).unwrap();
        assert_eq!(tenant, "foo");
    }

    #[test]
    fn header_resolver_defaults() {
        let routing = TenantRouting::new(RoutingConfig {
            resolver: TenantResolver::Header(HeaderName::from_static("x-tenant")),
            default_tenant: "demo".into(),
        });
        let (parts, _) = Request::builder()
            .uri("http://localhost")
            .body(())
            .unwrap()
            .into_parts();
        let tenant = routing.resolve(&parts).unwrap();
        assert_eq!(tenant, "demo");
    }

    #[test]
    fn from_env_with_default_uses_override() {
        let expected = std::env::var("DEFAULT_TENANT").unwrap_or_else(|_| "custom".into());
        let cfg = RoutingConfig::from_env_with_default("custom".into());
        assert_eq!(cfg.default_tenant, expected);
    }

    #[test]
    fn jwt_resolver_reads_tenant_claim() {
        let routing = TenantRouting::new(RoutingConfig {
            resolver: TenantResolver::Jwt {
                header: AUTHORIZATION,
                claim: "tenant".into(),
            },
            default_tenant: "demo".into(),
        });
        let payload = STANDARD.encode(br#"{"tenant":"jwt-tenant"}"#);
        let token = format!("ignored.{payload}.ignored");
        let (parts, _) = Request::builder()
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap()
            .into_parts();

        assert_eq!(routing.resolve(&parts).unwrap(), "jwt-tenant");
    }

    #[test]
    fn jwt_resolver_falls_back_on_invalid_payload() {
        let routing = TenantRouting::new(RoutingConfig {
            resolver: TenantResolver::Jwt {
                header: AUTHORIZATION,
                claim: "tenant".into(),
            },
            default_tenant: "demo".into(),
        });
        let (parts, _) = Request::builder()
            .header(AUTHORIZATION, "Bearer invalid.token.payload")
            .body(())
            .unwrap()
            .into_parts();

        assert_eq!(routing.resolve(&parts).unwrap(), "demo");
    }

    #[test]
    fn jwt_resolver_requires_bearer_prefix() {
        let routing = TenantRouting::new(RoutingConfig {
            resolver: TenantResolver::Jwt {
                header: AUTHORIZATION,
                claim: "tenant".into(),
            },
            default_tenant: "demo".into(),
        });
        let (parts, _) = Request::builder().body(()).unwrap().into_parts();

        assert!(routing.resolve(&parts).is_err());
    }
}
