//! DwApplication pack manifest → `AgentConfig` conversion. Shared by the
//! runner host (`gtc start`) and the designer so both apply identical rules.
use std::collections::BTreeMap;

use serde::Deserialize;

/// Minimal typed view of a designer-exported `DwApplication` `manifest.json`.
/// Tolerant of unknown fields so future pack additions don't break parsing.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DwApplicationManifest {
    pub manifest_id: String,
    #[serde(default)]
    manifest: DwManifestBody,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct DwManifestBody {
    #[serde(default)]
    capability_plan: DwCapabilityPlan,
    #[serde(default)]
    defaults: DwDefaults,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct DwCapabilityPlan {
    #[serde(default)]
    default_provider_ids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
struct DwDefaults {
    #[serde(default)]
    values: BTreeMap<String, serde_json::Value>,
}

impl DwApplicationManifest {
    /// Provider id bound to `cap://llm/chat`, if any.
    pub fn llm_provider_id(&self) -> Option<&str> {
        self.manifest
            .capability_plan
            .default_provider_ids
            .get("cap://llm/chat")
            .map(String::as_str)
    }

    /// Provider id bound to a memory capability (e.g. `cap://memory/long-term`).
    pub fn memory_provider_id(&self, cap: &str) -> Option<&str> {
        self.manifest
            .capability_plan
            .default_provider_ids
            .get(cap)
            .map(String::as_str)
    }

    /// The agent system prompt (`defaults.values.system_prompt`), or "".
    ///
    /// Returns `""` if the key is absent or the value is not a JSON string.
    pub fn system_prompt(&self) -> &str {
        self.manifest
            .defaults
            .values
            .get("system_prompt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    }

    /// Model default for a provider id (`defaults.values["{provider_id}::model"]`).
    pub fn model_for(&self, provider_id: &str) -> Option<&str> {
        let key = format!("{provider_id}::model");
        self.manifest
            .defaults
            .values
            .get(&key)
            .and_then(serde_json::Value::as_str)
    }
}

/// Map a catalog `provider_id` to its provider slug. Catalog ids follow
/// `provider.llm.{slug}.{variant}` (e.g. `provider.llm.deepseek.chat`); this
/// strips prefix + variant to `{slug}`. Anything not matching is returned
/// unchanged, so a pre-resolved slug like `"deepseek"` passes through.
#[must_use]
pub fn provider_slug(provider_id: &str) -> String {
    if let Some(rest) = provider_id.strip_prefix("provider.llm.")
        && let Some((slug, _variant)) = rest.split_once('.')
    {
        return slug.to_string();
    }
    provider_id.to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "manifest_id": "onboarding-companion",
      "display_name": "Onboarding Companion",
      "manifest": {
        "capability_plan": {
          "default_provider_ids": {
            "cap://llm/chat": "provider.llm.deepseek.chat",
            "cap://memory/long-term": "provider.memory.chronicle",
            "cap://memory/short-term": "provider.memory.redis"
          }
        },
        "defaults": {
          "values": {
            "system_prompt": "You are an Onboarding Companion.",
            "provider.llm.deepseek.chat::model": "deepseek-chat"
          }
        }
      },
      "tenant": "greentic"
    }"#;

    #[test]
    fn parses_manifest_fields() {
        let m: DwApplicationManifest = serde_json::from_str(FIXTURE).expect("parse");
        assert_eq!(m.manifest_id, "onboarding-companion");
        assert_eq!(m.llm_provider_id(), Some("provider.llm.deepseek.chat"));
        assert_eq!(
            m.memory_provider_id("cap://memory/long-term"),
            Some("provider.memory.chronicle")
        );
        assert_eq!(m.system_prompt(), "You are an Onboarding Companion.");
        assert_eq!(
            m.model_for("provider.llm.deepseek.chat"),
            Some("deepseek-chat")
        );
    }

    #[test]
    fn missing_manifest_subtree_defaults() {
        let m: DwApplicationManifest =
            serde_json::from_str(r#"{"manifest_id":"bare"}"#).expect("parse");
        assert_eq!(m.manifest_id, "bare");
        assert_eq!(m.llm_provider_id(), None);
        assert_eq!(m.memory_provider_id("cap://memory/long-term"), None);
        assert_eq!(m.system_prompt(), "");
        assert_eq!(m.model_for("x"), None);
    }

    #[test]
    fn absent_system_prompt_returns_empty() {
        let m: DwApplicationManifest = serde_json::from_str(
            r#"{"manifest_id":"test","manifest":{"defaults":{"values":{"other_key":"value"}}}}"#,
        )
        .expect("parse");
        assert_eq!(m.system_prompt(), "");
    }

    #[test]
    fn non_string_system_prompt_returns_empty() {
        let m: DwApplicationManifest = serde_json::from_str(
            r#"{"manifest_id":"test","manifest":{"defaults":{"values":{"system_prompt":42}}}}"#,
        )
        .expect("parse");
        assert_eq!(m.system_prompt(), "");
    }

    #[test]
    fn provider_slug_strips_catalog_prefix() {
        assert_eq!(provider_slug("provider.llm.deepseek.chat"), "deepseek");
        assert_eq!(provider_slug("provider.llm.anthropic.chat"), "anthropic");
        // pre-resolved slug passes through unchanged
        assert_eq!(provider_slug("deepseek"), "deepseek");
        // non-matching string passes through unchanged
        assert_eq!(provider_slug("provider.llm"), "provider.llm");
    }
}
