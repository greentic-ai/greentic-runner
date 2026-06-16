//! Per-agent configuration delivered by [`crate::ConfigProvider`].
//!
//! Defaults live in `AgentLimits::default()` per spec §5.2.
//! Override per-tenant via the admin designer config UI; the runtime
//! receives the resolved struct via the cached provider.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Reference to a tool exposed by a `greentic-ext-runtime` extension.
/// Composer extensions emit short names; runner-host derives full IDs
/// (extension_id + tool_name) via `ExtensionRuntime::list_tools` before
/// constructing the `AgentConfig` (see runner-host Phase 4).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRef {
    pub extension_id: String,
    pub tool_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmProviderRef {
    pub provider: String, // "openai" | "anthropic" | ...
    pub model: String,    // "gpt-4o-mini" | "claude-3-haiku" | ...
    /// Per-tenant credential identifier (admin provider UUID) used by the
    /// runner to resolve `secrets://default/{tenant}/_/llm/{credential_ref}`.
    /// `None` falls back to env-keyed credentials (legacy single-provider).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_ref: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: String,
    pub system_prompt: String,
    pub tools: Vec<ToolRef>,
    pub llm: LlmProviderRef,
    #[serde(default)]
    pub limits: AgentLimits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLimits {
    /// Maximum Plan-Act-Observe iterations per step. Default: 8.
    pub max_iter: u32,

    /// Wall-clock timeout for the entire `step()` call. Default: 60s.
    #[serde(with = "duration_secs")]
    pub timeout: Duration,

    /// Maximum retained conversation history turns. Default: 20.
    /// Truncation drops oldest user-assistant pairs; system prompt
    /// preserved.
    pub max_history_turns: u32,

    /// LLM retry attempts before declaring provider unavailable.
    /// Default: 3.
    pub llm_retry_attempts: u32,

    /// Initial backoff for LLM retries; exponential. Default: 250ms.
    #[serde(with = "duration_ms")]
    pub llm_retry_backoff: Duration,

    /// Tenant-configurable user-facing message when LLM is unavailable
    /// after all retries.
    pub provider_failure_message: Option<String>,

    /// Daily token cap per tenant. `None` = uncapped (MVP default).
    pub daily_token_cap_per_tenant: Option<u32>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_iter: 8,
            timeout: Duration::from_secs(60),
            max_history_turns: 20,
            llm_retry_attempts: 3,
            llm_retry_backoff: Duration::from_millis(250),
            provider_failure_message: None,
            daily_token_cap_per_tenant: None,
        }
    }
}

mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_secs(u64::deserialize(d)?))
    }
}

mod duration_ms {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        // as_millis() returns u128; AgentLimits durations are bounded
        // by operator config and never approach u64::MAX (~584M years).
        #[allow(clippy::cast_possible_truncation)]
        let ms = d.as_millis() as u64;
        s.serialize_u64(ms)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod limits_serde_tests {
    use super::*;

    #[test]
    fn agent_limits_fills_all_missing_fields_from_default() {
        let limits: AgentLimits = serde_json::from_str("{}").unwrap();
        assert_eq!(limits.max_iter, 8);
        assert_eq!(limits.timeout, std::time::Duration::from_secs(60));
        assert_eq!(limits.max_history_turns, 20);
        assert_eq!(limits.llm_retry_attempts, 3);
        assert_eq!(
            limits.llm_retry_backoff,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(limits.provider_failure_message, None);
        assert_eq!(limits.daily_token_cap_per_tenant, None);
    }

    #[test]
    fn agent_limits_partial_keeps_given_overrides_defaults_rest() {
        let limits: AgentLimits =
            serde_json::from_str(r#"{ "max_iter": 99, "timeout": 5 }"#).unwrap();
        assert_eq!(limits.max_iter, 99);
        assert_eq!(limits.timeout, std::time::Duration::from_secs(5));
        assert_eq!(limits.max_history_turns, 20);
        assert_eq!(limits.llm_retry_attempts, 3);
    }

    #[test]
    fn agent_config_allows_omitted_limits_block() {
        let json = r#"{
            "agent_id": "greeter",
            "system_prompt": "hi",
            "tools": [],
            "llm": { "provider": "openai", "model": "gpt-4o-mini" }
        }"#;
        let cfg: AgentConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.limits.max_iter, 8);
        assert_eq!(cfg.limits.timeout, std::time::Duration::from_secs(60));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn llm_provider_ref_defaults_credential_ref_to_none() {
        let r: LlmProviderRef =
            serde_json::from_str(r#"{ "provider":"anthropic","model":"claude-3" }"#).unwrap();
        assert_eq!(r.provider, "anthropic");
        assert_eq!(r.credential_ref, None);
    }

    #[test]
    fn defaults_match_spec_5_2() {
        let l = AgentLimits::default();
        assert_eq!(l.max_iter, 8);
        assert_eq!(l.timeout, Duration::from_secs(60));
        assert_eq!(l.max_history_turns, 20);
        assert_eq!(l.llm_retry_attempts, 3);
        assert_eq!(l.llm_retry_backoff, Duration::from_millis(250));
        assert!(l.provider_failure_message.is_none());
        assert!(l.daily_token_cap_per_tenant.is_none());
    }

    #[test]
    fn agent_config_roundtrips_through_json() {
        let original = AgentConfig {
            agent_id: "a-1".into(),
            system_prompt: "be helpful".into(),
            tools: vec![ToolRef {
                extension_id: "http".into(),
                tool_name: "fetch".into(),
            }],
            llm: LlmProviderRef {
                provider: "openai".into(),
                model: "gpt-4o-mini".into(),
                credential_ref: None,
            },
            limits: AgentLimits::default(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let round: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(round.agent_id, original.agent_id);
        assert_eq!(round.limits.max_iter, 8);
        assert_eq!(round.limits.timeout, Duration::from_secs(60));
        assert_eq!(round.limits.llm_retry_backoff, Duration::from_millis(250));
    }
}
