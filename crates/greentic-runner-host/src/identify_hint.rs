//! Phase D: per-provider identify-instance hint, consumed from the
//! `greentic:provider-instance-identity/instance-identity-describe@0.1.0`
//! WIT world.
//!
//! The hint lets the host scope the per-provider header allowlist when
//! building the M1 IID.4d wrapper, replacing the global
//! `IDENTIFY_HEADER_ALLOWLIST` in greentic-start. See the WIT docstring
//! on `instance-identity-describe-api.describe-identify-instance` for the
//! authoritative JSON shape and version contract.
//!
//! This module is pure JSON parsing — no Wasmtime or host dependencies.
//! The probe + cache live on [`crate::pack::PackRuntime`].
//!
//! Version contract: the top-level `"version"` field MUST equal `1`. A
//! missing or non-1 version is rejected as malformed and the caller
//! treats it as if no hint were exported (unhinted fallback). This
//! forward-compatibility gate matches the WIT requirement.

use serde::Deserialize;

/// Parsed per-provider hint declaring where this component's
/// identify-instance extracts the discriminator from. First match within
/// [`sources`](IdentifyInstanceHint::sources) wins (the host iterates the
/// sources in order; this module does not pick a winner — the caller does
/// when scoping the wrapper).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentifyInstanceHint {
    /// Ordered list of extraction sources. Empty is legal but degenerate
    /// (the hint says "nothing scoped"); callers treat it as no headers
    /// allowed for this provider.
    pub sources: Vec<HintSource>,
}

/// One extraction site for the discriminator. The two variants mirror
/// the WIT-side shape (`{ "header": { "name": "…" } }` and
/// `{ "body_path": { "json_pointer": "…" } }`). The host currently uses
/// [`HintSource::Header`] to derive the per-provider header allowlist;
/// [`HintSource::BodyPath`] is parsed but not yet acted on (the body is
/// always forwarded in full — short-circuit body extraction is a later
/// Phase D follow-up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintSource {
    /// Discriminator lives in an HTTP request header. `name` is the
    /// lowercase header name (matched against
    /// [`build_identify_payload`](crate::pack::PackRuntime)'s lowercased
    /// header keys).
    Header { name: String },
    /// Discriminator lives at `json_pointer` inside the request body. The
    /// pointer follows RFC 6901; the host does not yet validate the
    /// pointer at parse time — invalid pointers will simply fail to
    /// extract at probe time.
    BodyPath { json_pointer: String },
}

/// Outcome of parsing a JSON-encoded hint. The host treats a missing
/// export, an explicit `None` from a present export, malformed JSON, and
/// a non-1 `version` field identically: the provider is unhinted and the
/// caller falls back to the legacy (unscoped) wrapper construction. A
/// failed parse from this module is therefore a `None` return — the
/// caller deliberately discards the error after warning.
impl IdentifyInstanceHint {
    /// Parse the JSON returned by `describe-identify-instance`. Returns
    /// `Ok(Some(hint))` on a well-formed `version: 1` document,
    /// `Ok(None)` on a well-formed document whose sources list is empty
    /// AND no other constraints apply (kept as `Some(empty)` so callers
    /// distinguishing degenerate hints from unhinted callers stays
    /// explicit — see test `degenerate_empty_sources_round_trips`).
    /// Returns `Err` only when the bytes don't parse as JSON, when
    /// `version` is missing/non-numeric/non-1, or when the `sources`
    /// array contains an unknown variant.
    pub fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        let wire: WireHint = serde_json::from_slice(bytes)
            .map_err(|err| anyhow::anyhow!("identify-instance hint is not valid JSON: {err}"))?;
        if wire.version != 1 {
            anyhow::bail!(
                "identify-instance hint version {} is not supported (host accepts version=1 only)",
                wire.version
            );
        }
        let sources = wire
            .sources
            .into_iter()
            .map(|src| match src {
                WireSource::Header(inner) => HintSource::Header {
                    name: inner.name.to_ascii_lowercase(),
                },
                WireSource::BodyPath(inner) => HintSource::BodyPath {
                    json_pointer: inner.json_pointer,
                },
            })
            .collect();
        Ok(Self { sources })
    }

    /// Return the set of lowercase header names this hint declares as
    /// discriminator sources. Used by the per-provider wrapper builder to
    /// filter the input header list down before passing the wrapper to
    /// `identify-instance`. A hint with no [`HintSource::Header`] entries
    /// yields an empty set — that provider's wrapper carries no headers.
    pub fn header_names(&self) -> Vec<&str> {
        self.sources
            .iter()
            .filter_map(|src| match src {
                HintSource::Header { name } => Some(name.as_str()),
                HintSource::BodyPath { .. } => None,
            })
            .collect()
    }
}

// Wire types: kept private so callers go through `from_json`. Using
// `#[serde(deny_unknown_fields)]` on the top-level wire shape ensures a
// future field addition can't be silently dropped by an old host — we
// want to surface "this hint had something we don't understand" as a
// parse error, the same way the version gate does.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHint {
    version: u32,
    #[serde(default)]
    sources: Vec<WireSource>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum WireSource {
    Header(WireHeaderSource),
    BodyPath(WireBodyPathSource),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireHeaderSource {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBodyPathSource {
    json_pointer: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_telegram_shape_header_source() {
        let json = br#"{
            "version": 1,
            "sources": [
                { "header": { "name": "x-telegram-bot-api-secret-token" } }
            ]
        }"#;
        let hint = IdentifyInstanceHint::from_json(json).expect("valid telegram hint parses");
        assert_eq!(
            hint.sources,
            vec![HintSource::Header {
                name: "x-telegram-bot-api-secret-token".into(),
            }]
        );
        assert_eq!(hint.header_names(), vec!["x-telegram-bot-api-secret-token"]);
    }

    #[test]
    fn parses_teams_shape_body_path_source() {
        let json = br#"{
            "version": 1,
            "sources": [
                { "body_path": { "json_pointer": "/recipient/id" } }
            ]
        }"#;
        let hint = IdentifyInstanceHint::from_json(json).expect("valid teams hint parses");
        assert_eq!(
            hint.sources,
            vec![HintSource::BodyPath {
                json_pointer: "/recipient/id".into(),
            }]
        );
        // BodyPath contributes no headers to the scoped allowlist.
        assert!(hint.header_names().is_empty());
    }

    #[test]
    fn header_name_normalized_to_lowercase() {
        // HTTP header names are case-insensitive; the wrapper builder
        // matches on lowercase. Hints from components SHOULD be lowercase
        // but the host normalizes defensively so a typo'd uppercase entry
        // doesn't silently drop the header at filter time.
        let json = br#"{
            "version": 1,
            "sources": [
                { "header": { "name": "X-Telegram-Bot-Api-Secret-Token" } }
            ]
        }"#;
        let hint = IdentifyInstanceHint::from_json(json).expect("header normalizes");
        assert_eq!(hint.header_names(), vec!["x-telegram-bot-api-secret-token"]);
    }

    #[test]
    fn rejects_missing_version() {
        let json = br#"{ "sources": [] }"#;
        let err = IdentifyInstanceHint::from_json(json).expect_err("version is required");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid JSON") || msg.contains("missing field"),
            "expected version-required parse failure, got: {msg}"
        );
    }

    #[test]
    fn rejects_non_one_version() {
        let json = br#"{ "version": 2, "sources": [] }"#;
        let err = IdentifyInstanceHint::from_json(json).expect_err("version must be 1");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("version 2") && msg.contains("version=1"),
            "version gate should name the offered + expected versions, got: {msg}"
        );
    }

    #[test]
    fn rejects_unknown_source_variant() {
        // Future variants (e.g. compound `all_of`) MUST be surfaced as a
        // parse error so old hosts don't silently drop them — the WIT
        // docstring requires this. We test the rejection happens at the
        // wire level (deny_unknown_fields on the enum).
        let json = br#"{
            "version": 1,
            "sources": [ { "all_of": [] } ]
        }"#;
        let err = IdentifyInstanceHint::from_json(json).expect_err("unknown variant rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not valid JSON"),
            "unknown variant should fail parsing via deny_unknown_fields, got: {msg}"
        );
    }

    #[test]
    fn rejects_extra_top_level_field() {
        // Forward-compat gate: a future v1 extension MUST be a separate
        // version bump, not a silent field addition. deny_unknown_fields
        // enforces this at parse time.
        let json = br#"{
            "version": 1,
            "sources": [],
            "extra": "ignored?"
        }"#;
        let err = IdentifyInstanceHint::from_json(json).expect_err("extra field rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("not valid JSON"), "got: {msg}");
    }

    #[test]
    fn degenerate_empty_sources_round_trips() {
        // A component can ship a hint with an empty sources list. This is
        // explicitly distinct from no-hint: it says "I do identify but
        // I'll do it from inputs you haven't told me about" — the
        // wrapper will carry no headers and an empty body filter, leaving
        // the component to operate on the bare body shape. Surfacing this
        // as `Some(empty)` lets the caller make the back-compat choice
        // explicitly rather than collapsing to None and silently using
        // the global allowlist.
        let json = br#"{ "version": 1, "sources": [] }"#;
        let hint = IdentifyInstanceHint::from_json(json).expect("empty sources parses");
        assert!(hint.sources.is_empty());
        assert!(hint.header_names().is_empty());
    }

    #[test]
    fn mixed_header_and_body_path_sources() {
        // A provider may discriminate via either path. We preserve the
        // ordering for first-match-wins semantics at the body-path
        // short-circuit layer (future PR); for the header-scoping pass
        // ordering doesn't matter — only the set of header names.
        let json = br#"{
            "version": 1,
            "sources": [
                { "header": { "name": "x-routing-tag" } },
                { "body_path": { "json_pointer": "/bot_id" } }
            ]
        }"#;
        let hint = IdentifyInstanceHint::from_json(json).expect("mixed hint parses");
        assert_eq!(hint.sources.len(), 2);
        assert_eq!(hint.header_names(), vec!["x-routing-tag"]);
    }

    #[test]
    fn header_source_missing_name_is_rejected() {
        let json = br#"{
            "version": 1,
            "sources": [ { "header": {} } ]
        }"#;
        let err = IdentifyInstanceHint::from_json(json).expect_err("name required");
        assert!(format!("{err:#}").contains("not valid JSON"));
    }

    #[test]
    fn body_path_source_missing_pointer_is_rejected() {
        let json = br#"{
            "version": 1,
            "sources": [ { "body_path": {} } ]
        }"#;
        let err = IdentifyInstanceHint::from_json(json).expect_err("pointer required");
        assert!(format!("{err:#}").contains("not valid JSON"));
    }
}
