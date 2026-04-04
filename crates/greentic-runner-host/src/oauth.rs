use anyhow::{Result, bail};
use greentic_types::TenantCtx;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use wasmtime::component::Linker;

#[derive(Clone, Debug, Default)]
pub struct OAuthBrokerConfig {
    pub http_base_url: String,
    pub nats_url: String,
    pub default_provider: Option<String>,
    pub team: Option<String>,
}

impl OAuthBrokerConfig {
    pub fn new(http_base_url: impl Into<String>, nats_url: impl Into<String>) -> Self {
        Self {
            http_base_url: http_base_url.into(),
            nats_url: nats_url.into(),
            default_provider: None,
            team: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OAuthBrokerHost;

pub trait OAuthHostContext {
    fn tenant_id(&self) -> &str;
    fn env(&self) -> &str;
    fn oauth_broker_host(&mut self) -> &mut OAuthBrokerHost;
    fn oauth_config(&self) -> Option<&OAuthBrokerConfig>;
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceTokenRequest {
    pub http_base_url: String,
    pub env: String,
    pub tenant: String,
    pub team: Option<String>,
    pub resource_id: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ResourceTokenResponse {
    pub access_token: String,
    pub expires_at: u64,
}

pub fn build_resource_token_request(
    config: &OAuthBrokerConfig,
    tenant: &TenantCtx,
    resource_id: &str,
    scopes: &[String],
) -> Result<ResourceTokenRequest> {
    if resource_id.trim().is_empty() {
        bail!("resource token request requires non-empty resource_id");
    }
    if scopes.is_empty() {
        bail!("resource token request requires at least one scope");
    }
    validate_https_base_url(&config.http_base_url)?;
    Ok(ResourceTokenRequest {
        http_base_url: config.http_base_url.clone(),
        env: tenant.env.to_string(),
        tenant: tenant.tenant.to_string(),
        team: tenant
            .team
            .as_ref()
            .map(|team| team.to_string())
            .or_else(|| config.team.clone()),
        resource_id: resource_id.to_string(),
        scopes: scopes.to_vec(),
    })
}

pub async fn request_resource_token(
    client: &reqwest::Client,
    request: &ResourceTokenRequest,
) -> Result<ResourceTokenResponse> {
    let base = Url::parse(&request.http_base_url)?;
    if base.scheme() != "https" {
        bail!(
            "oauth broker http_base_url must use https, got scheme `{}`",
            base.scheme()
        );
    }
    if !base.username().is_empty() || base.password().is_some() {
        bail!("oauth broker http_base_url must not include URL credentials");
    }
    let url = base.join("resource-token")?;
    let response = client
        .post(url)
        .json(request)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.json().await?)
}

fn validate_https_base_url(url: &str) -> Result<()> {
    let parsed = Url::parse(url)?;
    if parsed.scheme() != "https" {
        bail!(
            "oauth broker http_base_url must use https, got scheme `{}`",
            parsed.scheme()
        );
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("oauth broker http_base_url must not include URL credentials");
    }
    Ok(())
}

pub fn add_oauth_broker_to_linker<T>(_linker: &mut Linker<T>) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use greentic_types::{EnvId, TeamId, TenantId};
    use std::str::FromStr;

    fn sample_tenant() -> TenantCtx {
        TenantCtx::new(
            EnvId::from_str("dev").unwrap(),
            TenantId::from_str("acme").unwrap(),
        )
        .with_team(Some(TeamId::from_str("core").unwrap()))
    }

    #[test]
    fn builds_resource_token_request_from_config_and_tenant() {
        let mut cfg = OAuthBrokerConfig::new("https://oauth.example", "nats://localhost:4222");
        cfg.team = Some("ops".into());
        let tenant = sample_tenant();
        let request = build_resource_token_request(
            &cfg,
            &tenant,
            "msgraph-email",
            &["https://graph.microsoft.com/.default".into()],
        )
        .expect("request");

        assert_eq!(request.http_base_url, "https://oauth.example");
        assert_eq!(request.env, "dev");
        assert_eq!(request.tenant, "acme");
        assert_eq!(request.team, Some("core".into()));
        assert_eq!(request.resource_id, "msgraph-email");
        assert_eq!(
            request.scopes,
            vec!["https://graph.microsoft.com/.default".to_string()]
        );
    }

    #[test]
    fn rejects_empty_resource_id() {
        let cfg = OAuthBrokerConfig::new("https://oauth.example", "nats://localhost:4222");
        let tenant = sample_tenant();
        let err = build_resource_token_request(&cfg, &tenant, "", &["scope".into()])
            .expect_err("empty resource id should fail");
        assert!(err.to_string().contains("resource_id"));
    }

    #[test]
    fn rejects_empty_scopes() {
        let cfg = OAuthBrokerConfig::new("https://oauth.example", "nats://localhost:4222");
        let tenant = sample_tenant();
        let err = build_resource_token_request(&cfg, &tenant, "msgraph-email", &[])
            .expect_err("empty scopes should fail");
        assert!(err.to_string().contains("scope"));
    }

    #[test]
    fn resource_token_url_is_joined_under_base() {
        let cfg = OAuthBrokerConfig::new("https://oauth.example/api/", "nats://localhost:4222");
        let tenant = sample_tenant();
        let request = build_resource_token_request(
            &cfg,
            &tenant,
            "msgraph-email",
            &["https://graph.microsoft.com/.default".into()],
        )
        .expect("request");
        let base = Url::parse(&request.http_base_url).expect("base url");
        let url = base.join("resource-token").expect("joined url");
        assert_eq!(url.as_str(), "https://oauth.example/api/resource-token");
    }

    #[test]
    fn rejects_non_https_oauth_broker_base_url() {
        let cfg = OAuthBrokerConfig::new("http://oauth.example", "nats://localhost:4222");
        let tenant = sample_tenant();
        let err = build_resource_token_request(&cfg, &tenant, "msgraph-email", &["scope".into()])
            .expect_err("http oauth broker url should fail");
        assert!(err.to_string().contains("must use https"));
    }

    #[test]
    fn rejects_oauth_broker_base_url_with_userinfo() {
        let cfg =
            OAuthBrokerConfig::new("https://user:pass@oauth.example", "nats://localhost:4222");
        let tenant = sample_tenant();
        let err = build_resource_token_request(&cfg, &tenant, "msgraph-email", &["scope".into()])
            .expect_err("userinfo in oauth broker url should fail");
        assert!(err.to_string().contains("must not include URL credentials"));
    }
}
