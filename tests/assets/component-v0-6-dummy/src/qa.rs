use greentic_types::cbor::canonical;
use greentic_types::i18n_text::I18nText;
use greentic_types::schemas::component::v0_6_0::{QaMode, Question};
use serde_json::{Value as JsonValue, json};

// Internal normalized lifecycle semantics used by scaffolded QA operations.
// Input compatibility accepts legacy/provision aliases via `normalize_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizedMode {
    Setup,
    Update,
    Remove,
}

impl NormalizedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Update => "update",
            Self::Remove => "remove",
        }
    }
}

// Compatibility mapping for mode strings from operator/flow payloads.
pub fn normalize_mode(raw: &str) -> Option<NormalizedMode> {
    match raw {
        "default" | "setup" | "install" => Some(NormalizedMode::Setup),
        "update" | "upgrade" => Some(NormalizedMode::Update),
        "remove" => Some(NormalizedMode::Remove),
        _ => None,
    }
}

// Primary QA authoring entrypoint.
// Extend question sets here for your real setup/update/remove requirements.
pub fn qa_spec_cbor(mode: NormalizedMode) -> Vec<u8> {
    canonical::to_canonical_cbor_allow_floats(&qa_spec_json(mode)).unwrap_or_default()
}

pub fn qa_spec_json(mode: NormalizedMode) -> JsonValue {
    let (title_key, description_key, questions) = match mode {
        NormalizedMode::Setup => (
            "qa.install.title",
            Some("qa.install.description"),
            vec![
                question(
                    "api_key",
                    "qa.field.api_key.label",
                    "qa.field.api_key.help",
                    true,
                ),
                question(
                    "region",
                    "qa.field.region.label",
                    "qa.field.region.help",
                    true,
                ),
                question(
                    "webhook_base_url",
                    "qa.field.webhook_base_url.label",
                    "qa.field.webhook_base_url.help",
                    true,
                ),
                question(
                    "enabled",
                    "qa.field.enabled.label",
                    "qa.field.enabled.help",
                    false,
                ),
            ],
        ),
        NormalizedMode::Update => (
            "qa.update.title",
            Some("qa.update.description"),
            vec![
                question(
                    "api_key",
                    "qa.field.api_key.label",
                    "qa.field.api_key.help",
                    false,
                ),
                question(
                    "region",
                    "qa.field.region.label",
                    "qa.field.region.help",
                    false,
                ),
                question(
                    "webhook_base_url",
                    "qa.field.webhook_base_url.label",
                    "qa.field.webhook_base_url.help",
                    false,
                ),
                question(
                    "enabled",
                    "qa.field.enabled.label",
                    "qa.field.enabled.help",
                    false,
                ),
            ],
        ),
        NormalizedMode::Remove => (
            "qa.remove.title",
            Some("qa.remove.description"),
            vec![question(
                "confirm_remove",
                "qa.field.confirm_remove.label",
                "qa.field.confirm_remove.help",
                true,
            )],
        ),
    };

    json!({
        "mode": match mode {
            NormalizedMode::Setup => QaMode::Setup,
            NormalizedMode::Update => QaMode::Update,
            NormalizedMode::Remove => QaMode::Remove,
        },
        "title": I18nText::new(title_key, None),
        "description": description_key.map(|key| I18nText::new(key, None)),
        "questions": questions,
        "defaults": {}
    })
}

pub fn apply_answers_cbor(mode: NormalizedMode, current_config: &[u8], answers: &[u8]) -> Vec<u8> {
    let payload = json!({
        "current_config": decode_json_or_empty(current_config),
        "answers": decode_json_or_empty(answers),
    });
    canonical::to_canonical_cbor_allow_floats(&apply_answers(mode, &payload)).unwrap_or_default()
}

fn question(id: &str, label_key: &str, help_key: &str, required: bool) -> Question {
    serde_json::from_value(json!({
        "id": id,
        "label": I18nText::new(label_key, None),
        "help": I18nText::new(help_key, None),
        "error": null,
        "kind": { "type": "text" },
        "required": required,
        "default": null
    }))
    .expect("question should deserialize")
}

// Used by `i18n-keys` operation and contract checks in operator.
pub fn i18n_keys() -> Vec<String> {
    crate::i18n::all_keys()
}

// Apply answers and return operator-friendly base shape:
// { ok, config?, warnings, errors, ...optional metadata }
// Extend this method for domain validation rules and config patching.
pub fn apply_answers(mode: NormalizedMode, payload: &JsonValue) -> JsonValue {
    let answers = payload.get("answers").cloned().unwrap_or_else(|| json!({}));
    let current_config = payload
        .get("current_config")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let mut errors = Vec::new();
    match mode {
        NormalizedMode::Setup => {
            for key in ["api_key", "region", "webhook_base_url"] {
                if answers.get(key).and_then(|v| v.as_str()).is_none() {
                    errors.push(json!({
                        "key": "qa.error.required",
                        "msg_key": "qa.error.required",
                        "fields": [key]
                    }));
                }
            }
        }
        NormalizedMode::Remove => {
            if answers
                .get("confirm_remove")
                .and_then(|v| v.as_str())
                .map(|v| v != "true")
                .unwrap_or(true)
            {
                errors.push(json!({
                    "key": "qa.error.remove_confirmation",
                    "msg_key": "qa.error.remove_confirmation",
                    "fields": ["confirm_remove"]
                }));
            }
        }
        NormalizedMode::Update => {}
    }

    if !errors.is_empty() {
        return json!({
            "ok": false,
            "warnings": [],
            "errors": errors,
            "meta": {
                "mode": mode.as_str(),
                "version": "v1"
            }
        });
    }

    let mut config = match current_config {
        JsonValue::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    if let JsonValue::Object(map) = answers {
        for (key, value) in map {
            config.insert(key, value);
        }
    }
    if mode == NormalizedMode::Remove {
        config.insert("enabled".to_string(), JsonValue::Bool(false));
    }

    json!({
        "ok": true,
        "config": config,
        "warnings": [],
        "errors": [],
        "meta": {
            "mode": mode.as_str(),
            "version": "v1"
        },
        "audit": {
            "reasons": ["qa.apply_answers"],
            "timings_ms": {}
        }
    })
}

fn decode_json_or_empty(bytes: &[u8]) -> JsonValue {
    if let Ok(value) = canonical::from_cbor(bytes) {
        return value;
    }
    serde_json::from_slice(bytes).unwrap_or_else(|_| json!({}))
}
