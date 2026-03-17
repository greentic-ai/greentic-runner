//! I18n message catalog for card translation resolution.
//!
//! Compatible with `messaging_cardkit::i18n::I18nCatalog`.
//! Stores translation messages keyed by dotted key path (e.g., "card.title").
//!
//! # Pack Structure
//!
//! I18n bundles are stored in `assets/i18n/{locale}.json`:
//! ```text
//! my-pack/
//! ├── assets/
//! │   └── i18n/
//! │       ├── en.json
//! │       ├── ar.json
//! │       └── ar-SA.json
//! ```
//!
//! # Bundle Format
//!
//! ```json
//! {
//!   "card.title": "Welcome",
//!   "card.items.count.one": "{count} item",
//!   "card.items.count.other": "{count} items"
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// I18n catalog for translation resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct I18nCatalog {
    /// Current locale (e.g., "ar-SA", "en-US").
    pub locale: String,
    /// Translation messages keyed by dotted key path.
    pub messages: HashMap<String, String>,
    /// Optional fallback messages (usually English).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<HashMap<String, String>>,
}

impl I18nCatalog {
    /// Create a new catalog for a locale.
    pub fn new(locale: impl Into<String>) -> Self {
        Self {
            locale: locale.into(),
            messages: HashMap::new(),
            fallback: None,
        }
    }

    /// Create a catalog from a JSON object.
    ///
    /// Expected format: `{ "key.path.here": "Translation value" }`
    pub fn from_json(locale: &str, json: &Value) -> Result<Self, String> {
        let obj = json
            .as_object()
            .ok_or_else(|| "i18n bundle must be a JSON object".to_string())?;

        let mut messages = HashMap::new();
        for (key, value) in obj {
            if let Value::String(text) = value {
                messages.insert(key.clone(), text.clone());
            }
        }

        Ok(Self {
            locale: locale.to_string(),
            messages,
            fallback: None,
        })
    }

    /// Add fallback messages (typically English).
    pub fn with_fallback(mut self, fallback: HashMap<String, String>) -> Self {
        self.fallback = Some(fallback);
        self
    }

    /// Look up a translation key.
    ///
    /// Resolution order: primary messages → fallback messages → None
    pub fn get(&self, key: &str) -> Option<&str> {
        self.messages
            .get(key)
            .map(String::as_str)
            .or_else(|| self.fallback.as_ref()?.get(key).map(String::as_str))
    }
}
