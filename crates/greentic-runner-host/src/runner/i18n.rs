//! Locale resolution + message lookup helpers used across the runner host.
//!
//! Vendored inline when greentic-i18n 0.5 dropped its library surface: the
//! crate is binary-only on crates.io now, and the lower-level greentic-i18n-lib
//! does not expose these runner-specific helpers. Semantics intentionally
//! match the 0.4 greentic-i18n lib so resolved locales and message keys are
//! unchanged.

use std::env;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct I18nText {
    pub message_key: String,
    pub fallback: String,
}

impl I18nText {
    pub fn new(message_key: impl Into<String>, fallback: impl Into<String>) -> Self {
        Self {
            message_key: message_key.into(),
            fallback: fallback.into(),
        }
    }
}

pub fn normalize_locale(value: &str) -> String {
    let lower = value.replace('_', "-").to_ascii_lowercase();
    match lower.split('-').next() {
        Some("en") => "en".to_string(),
        Some(primary) if !primary.is_empty() => primary.to_string(),
        _ => "en".to_string(),
    }
}

pub fn select_locale_with_sources(
    cli_locale: Option<&str>,
    explicit: Option<&str>,
    env_locale: Option<&str>,
    system_locale: Option<&str>,
) -> String {
    if let Some(value) = cli_locale.map(str::trim).filter(|value| !value.is_empty()) {
        return normalize_locale(value);
    }
    if let Some(value) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return normalize_locale(value);
    }
    if let Some(value) = env_locale.map(str::trim).filter(|value| !value.is_empty()) {
        return normalize_locale(value);
    }
    if let Some(value) = system_locale
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return normalize_locale(value);
    }
    "en".to_string()
}

pub fn select_locale(explicit: Option<&str>) -> String {
    let cli_locale = env::var("GREENTIC_LOCALE_CLI").ok();
    let env_locale = env::var("GREENTIC_LOCALE").ok();
    let system = system_locale();
    select_locale_with_sources(
        cli_locale.as_deref(),
        explicit,
        env_locale.as_deref(),
        system.as_deref(),
    )
}

pub fn resolve_message(key: &str, fallback: &str, locale: &str) -> String {
    let normalized = normalize_locale(locale);
    match normalized.as_str() {
        "en" => english_message(key).unwrap_or(fallback).to_string(),
        _ => fallback.to_string(),
    }
}

pub fn resolve_text(text: &I18nText, locale: &str) -> String {
    resolve_message(&text.message_key, &text.fallback, locale)
}

fn english_message(key: &str) -> Option<&'static str> {
    match key {
        "runner.operator.schema_hash_mismatch" => {
            Some("schema hash mismatch between request and resolved contract")
        }
        "runner.operator.contract_introspection_failed" => {
            Some("failed to introspect component contract")
        }
        "runner.operator.schema_ref_not_found" => Some("referenced schema not found in pack"),
        "runner.operator.schema_load_failed" => Some("failed to load referenced schema"),
        "runner.operator.new_state_schema_missing" => {
            Some("missing config schema required for new_state validation")
        }
        "runner.operator.new_state_schema_load_failed" => {
            Some("failed to load config schema for new_state validation")
        }
        "runner.operator.new_state_schema_unavailable" => {
            Some("new_state schema unavailable in strict mode")
        }
        "runner.operator.tenant_mismatch" => Some("request tenant does not match routed tenant"),
        "runner.operator.missing_provider_selector" => {
            Some("request must include provider_id or provider_type")
        }
        "runner.operator.provider_not_found" => Some("provider not found"),
        "runner.operator.op_not_found" => Some("operation not found"),
        "runner.operator.resolve_error" => Some("failed to resolve provider operation"),
        "runner.schema.unsupported_constraint" => Some("schema includes unsupported constraint"),
        "runner.schema.invalid_schema" => Some("invalid schema document"),
        "runner.schema.validation_failed" => Some("schema validation failed"),
        _ => None,
    }
}

fn system_locale() -> Option<String> {
    for key in ["LC_ALL", "LANG", "LC_MESSAGES"] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                continue;
            }
            let stripped = trimmed.split('.').next().unwrap_or(trimmed);
            if !stripped.is_empty() {
                return Some(normalize_locale(stripped));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_message_uses_fallback_for_unknown_key() {
        let message = resolve_message("runner.unknown", "fallback message", "en");
        assert_eq!(message, "fallback message");
    }

    #[test]
    fn resolve_text_uses_key_and_fallback() {
        let text = I18nText::new("runner.operator.op_not_found", "fallback");
        let message = resolve_text(&text, "en");
        assert_eq!(message, "operation not found");
    }

    #[test]
    fn normalize_locale_reduces_variants() {
        assert_eq!(normalize_locale("en-US"), "en");
        assert_eq!(normalize_locale("nl_NL"), "nl");
    }

    #[test]
    fn select_locale_prefers_explicit_over_env_and_system() {
        assert_eq!(
            select_locale_with_sources(None, Some("en-US"), Some("fr-FR"), Some("nl_NL.UTF-8")),
            "en"
        );
    }

    #[test]
    fn select_locale_prefers_cli_override() {
        assert_eq!(
            select_locale_with_sources(
                Some("it-IT"),
                Some("en-US"),
                Some("fr-FR"),
                Some("nl_NL.UTF-8")
            ),
            "it"
        );
    }

    #[test]
    fn select_locale_uses_env_over_system() {
        assert_eq!(
            select_locale_with_sources(None, None, Some("de-DE"), Some("nl_NL.UTF-8")),
            "de"
        );
    }

    #[test]
    fn select_locale_falls_back_to_system_then_en() {
        assert_eq!(
            select_locale_with_sources(None, None, None, Some("es_ES.UTF-8")),
            "es"
        );
        assert_eq!(select_locale_with_sources(None, None, None, None), "en");
    }
}
