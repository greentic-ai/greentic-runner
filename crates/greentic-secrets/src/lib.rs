use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use tracing::info;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretsBackend {
    Env,
    Aws,
    Gcp,
    Azure,
}

impl SecretsBackend {
    pub fn from_env(env_value: Option<String>) -> Result<Self> {
        match env_value {
            Some(raw) => raw.parse(),
            None => Ok(SecretsBackend::Env),
        }
    }
}

impl FromStr for SecretsBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "env" => Ok(SecretsBackend::Env),
            "aws" => Ok(SecretsBackend::Aws),
            "gcp" => Ok(SecretsBackend::Gcp),
            "azure" | "az" => Ok(SecretsBackend::Azure),
            "" => Ok(SecretsBackend::Env),
            other => bail!("unsupported secrets backend `{other}`"),
        }
    }
}

impl fmt::Display for SecretsBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretsBackend::Env => write!(f, "env"),
            SecretsBackend::Aws => write!(f, "aws"),
            SecretsBackend::Gcp => write!(f, "gcp"),
            SecretsBackend::Azure => write!(f, "azure"),
        }
    }
}

/// Initialises the requested secrets backend. The current implementation
/// only validates the configuration and records the intent so higher-level
/// runners can swap in provider-specific integrations later on.
pub fn init(backend: SecretsBackend) -> Result<()> {
    match backend {
        SecretsBackend::Env => {
            info!("secrets backend=env (environment variables)");
            Ok(())
        }
        other => {
            info!(backend = %other, "secrets backend initialised (placeholder)");
            // TODO: wire provider SDKs once available.
            Ok(())
        }
    }
}
