//! `BrokerSecretsManager` — reads/writes per-tenant secrets over the
//! greentic-secrets-broker HTTP API. The runner uses this to resolve
//! per-tenant LLM credentials written by greentic-designer-admin.

use greentic_secrets_lib::SecretError;
use reqwest::Client;
use serde::Deserialize;

/// Convert a canonical `secrets://{env}/{tenant}/{team}/{cat}/{name}` URI into
/// the broker `/v1/...` request path. Path segments are preserved verbatim
/// (the broker treats `_` as the tenant-wide team).
#[allow(dead_code)]
pub(crate) fn broker_path_from_uri(uri: &str) -> Result<String, SecretError> {
    let rest = uri
        .strip_prefix("secrets://")
        .ok_or_else(|| SecretError::Backend("not a secrets:// uri".into()))?;
    if rest.is_empty() {
        return Err(SecretError::Backend("empty secrets uri".into()));
    }
    Ok(format!("/v1/{rest}"))
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct SecretResponse {
    value: String,
    #[serde(default)]
    encoding: String, // "utf8" | "base64"
}

#[allow(dead_code)]
pub(crate) struct BrokerSecretsManager {
    client: Client,
    endpoint: String,
    token: String,
}

impl BrokerSecretsManager {
    #[allow(dead_code)]
    pub(crate) fn new(endpoint: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_secrets_uri_to_broker_v1_path() {
        let p = broker_path_from_uri("secrets://default/acme/_/llm/abc-123").unwrap();
        assert_eq!(p, "/v1/default/acme/_/llm/abc-123");
    }

    #[test]
    fn rejects_non_secrets_uri() {
        assert!(broker_path_from_uri("https://x/y").is_err());
    }
}
