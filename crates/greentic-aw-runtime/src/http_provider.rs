//! A [`ConfigProvider`] that pulls a full [`AgentConfig`] from the
//! greentic-designer-admin agent registry over HTTP, authed with a tenant
//! `gtc_live_*` bearer token. Tenant is implied by the token; `tenant` arg is
//! accepted for the trait but not used for the request.

use std::future::Future;
use std::pin::Pin;

use crate::config::AgentConfig;
use crate::config_provider::ConfigProvider;
use crate::error::ConfigError;
use crate::tenant::TenantContext;

/// Pulls `AgentConfig` from `{base}/api/v1/designer/agents/{agent_id}`.
pub struct HttpConfigProvider {
    base_url: String,
    token: String,
    client: reqwest::Client,
}

impl HttpConfigProvider {
    /// `base_url` is the admin origin (no trailing slash needed); `token` is a
    /// tenant `gtc_live_*` key.
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }
}

impl ConfigProvider for HttpConfigProvider {
    fn agent_config<'a>(
        &'a self,
        _tenant: &'a TenantContext,
        agent_id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<AgentConfig, ConfigError>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!("{}/api/v1/designer/agents/{agent_id}", self.base_url);
            let resp = self
                .client
                .get(&url)
                .bearer_auth(&self.token)
                .send()
                .await
                .map_err(|e| {
                    ConfigError::Internal(format!("agent registry request failed: {e}"))
                })?;

            match resp.status().as_u16() {
                200 => resp
                    .json::<AgentConfig>()
                    .await
                    .map_err(|e| ConfigError::Misconfigured(format!("agent config decode: {e}"))),
                404 => Err(ConfigError::AgentNotFound(agent_id.to_string())),
                other => Err(ConfigError::Internal(format!(
                    "agent registry returned status {other}"
                ))),
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn agent_config_json() -> serde_json::Value {
        serde_json::json!({
            "agent_id": "bot",
            "system_prompt": "be helpful",
            "tools": [{ "extension_id": "greentic.tavily", "tool_name": "web_search" }],
            "llm": { "provider": "openai", "model": "gpt-4o-mini" },
            "limits": {}
        })
    }

    #[tokio::test]
    async fn fetches_and_parses_agent_config() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/designer/agents/bot"))
            .and(header("authorization", "Bearer gtc_live_x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(agent_config_json()))
            .mount(&server)
            .await;

        let provider = HttpConfigProvider::new(server.uri(), "gtc_live_x");
        let tenant = TenantContext::new("t", "e");
        let cfg = provider.agent_config(&tenant, "bot").await.unwrap();
        assert_eq!(cfg.agent_id, "bot");
        assert_eq!(cfg.llm.model, "gpt-4o-mini");
        assert_eq!(cfg.tools.len(), 1);
    }

    #[tokio::test]
    async fn maps_404_to_agent_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let provider = HttpConfigProvider::new(server.uri(), "gtc_live_x");
        let tenant = TenantContext::new("t", "e");
        let result = provider.agent_config(&tenant, "ghost").await;
        assert!(matches!(result, Err(ConfigError::AgentNotFound(_))));
    }

    #[tokio::test]
    async fn maps_5xx_to_internal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let provider = HttpConfigProvider::new(server.uri(), "gtc_live_x");
        let tenant = TenantContext::new("t", "e");
        let result = provider.agent_config(&tenant, "bot").await;
        assert!(matches!(result, Err(ConfigError::Internal(_))));
    }
}
