//! Host adapter for the `greentic:extension-provider` worlds.
//!
//! Wave-4 C1 of the extension-error v2 epic. This module hosts the
//! `extension-provider` worlds at BOTH `@0.2.0` and `@0.1.0`, alongside the
//! existing legacy `greentic:provider*@1.0.0` schema-core bindings in
//! [`crate::provider_core`]. It is purely additive: nothing here changes the
//! legacy provider dispatch path, which stays the default for existing packs.
//!
//! # World shape (important)
//!
//! Unlike the legacy `schema-core` world — which exports a generic
//! `invoke(op, input-json) -> bytes` data-plane call — the
//! `extension-provider` worlds export *introspection* interfaces:
//!
//! - `messaging` — `list-channels`, `describe-channel`, `secret-schema`,
//!   `config-schema`, `dry-run-encode`
//! - `event-source` — `list-trigger-types`, `describe-trigger`,
//!   `trigger-schema`
//! - `event-sink` — `list-event-types`, `describe-event`, `event-schema`
//!
//! There is no runtime `invoke(op, input)` export on these worlds, so the
//! existing [`crate::pack::PackRuntime::invoke_provider`] data-plane call has
//! no `extension-provider` equivalent. See the `NEEDS_DECISION` note in the
//! PR body for how a pack would *declare* an extension-provider component and
//! how (or whether) the data plane should route to it. This module therefore
//! delivers the self-contained pieces the guardrail asks for — dual-world
//! bindgen, a probe, and typed error preservation — without inventing pack /
//! manifest schema.
//!
//! # Probe order
//!
//! [`probe_provider_world`] resolves the newest world first
//! (`extension-provider@0.2.0` via [`ProviderWorld::V0_2_0`]), then
//! `@0.1.0` ([`ProviderWorld::V0_1_0`]), by checking for the version-suffixed
//! exported interface instances with `get_export_index`. A `None` result
//! means the component is not an extension-provider at all and the caller
//! should fall back to the legacy schema-core detection in
//! [`crate::pack`].

#![allow(clippy::allow_attributes)]

use wasmtime::component::Instance;

// ---------------------------------------------------------------------------
// Bindgen — one module per generation.
//
// Each world imports `extension-host/logging@0.1.0` + `i18n@0.1.0` and
// `extension-base/types`, and exports `extension-base/{manifest,lifecycle}`
// plus the provider introspection interfaces. We bind the `full-provider`
// world (the superset exporting messaging + event-source + event-sink); a
// component that only implements a subset still type-checks against the
// import side, and missing exports are resolved lazily via
// `get_export_index` at call time rather than at instantiation.
//
// The two generations share package *names* but not versions, so they must
// live in sibling modules to avoid duplicate-type collisions (same pattern
// as `host_bindings.rs` in greentic-ext-runtime and `provider_core.rs`
// here).
// ---------------------------------------------------------------------------

/// `extension-provider@0.2.0` bindings (rev dda7974, v1.2.20-research).
///
/// Vendored under `wit-vendor/` (not the crate's default `wit/`) so the
/// inline `bindgen!` macros in `component_api.rs` / `provider_core.rs` keep
/// resolving an absent default `wit/` directory — a populated `wit/` would
/// break their default-path validation.
pub mod v0_2_0 {
    wasmtime::component::bindgen!({
        path: "wit-vendor/extension-provider/v0_2_0",
        world: "greentic:extension-provider/full-provider",
    });
}

/// `extension-provider@0.1.0` bindings (rev 8c619b7).
pub mod v0_1_0 {
    wasmtime::component::bindgen!({
        path: "wit-vendor/extension-provider/v0_1_0",
        world: "greentic:extension-provider/full-provider",
    });
}

/// Which `extension-provider` world generation a component was built against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderWorld {
    /// `greentic:extension-provider@0.2.0` — typed 6-variant `extension-error`.
    V0_2_0,
    /// `greentic:extension-provider@0.1.0` — 3-variant provider `error`.
    V0_1_0,
}

impl ProviderWorld {
    /// The version suffix used on exported interface instance names.
    pub const fn version(self) -> &'static str {
        match self {
            ProviderWorld::V0_2_0 => "0.2.0",
            ProviderWorld::V0_1_0 => "0.1.0",
        }
    }
}

/// Probe an instantiated component for an `extension-provider` world,
/// newest-first.
///
/// Returns the highest world generation whose `messaging` interface is
/// exported, or `None` when the component exports no recognised
/// extension-provider interface (in which case the caller should fall back
/// to legacy schema-core detection).
///
/// We probe the `messaging` interface as the discriminator because every
/// provider world that matters today exports it, and because it is the only
/// interface shared by the messaging/full provider worlds. The
/// `event-source`/`event-sink` interfaces are resolved on demand by the
/// individual call sites once the world generation is known.
pub fn probe_provider_world(
    store: &mut wasmtime::Store<impl Send>,
    instance: &Instance,
) -> Option<ProviderWorld> {
    for world in [ProviderWorld::V0_2_0, ProviderWorld::V0_1_0] {
        let iface = format!("greentic:extension-provider/messaging@{}", world.version());
        if instance
            .get_export_index(&mut *store, None, &iface)
            .is_some()
        {
            return Some(world);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Typed error preservation
// ---------------------------------------------------------------------------

/// Host-side, version-agnostic provider invocation error.
///
/// Mirrors the canonical six-variant `greentic:extension-base/types@0.2.0`
/// `extension-error` and is structurally identical to
/// `greentic-ext-runtime`'s `HostExtensionError`. It is intentionally defined
/// *locally* rather than imported from `greentic-ext-runtime` because that
/// dependency is gated behind the optional `agentic-worker` feature
/// (`Cargo.toml`: `agentic-worker = ["dep:greentic-aw-runtime",
/// "dep:greentic-ext-runtime"]`). Pulling the whole feature in just to reuse a
/// four-field error enum would force `greentic-aw-runtime` into lean,
/// `agentic-worker`-off builds that deliberately exclude DwAgent support.
/// A small local enum keeps the provider adapter usable in every build
/// configuration while preserving the exact same wire codes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderInvokeError {
    /// The request payload was malformed or failed validation.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// The component lacks a capability required to service the request.
    #[error("missing capability: {0}")]
    MissingCapability(String),
    /// The component refused the request on policy grounds.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// The referenced resource (channel / trigger / event) does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A schema document supplied or produced by the component was invalid.
    #[error("schema invalid: {0}")]
    SchemaInvalid(String),
    /// An unclassified internal failure.
    #[error("internal: {0}")]
    Internal(String),
}

impl ProviderInvokeError {
    /// Stable kebab-case wire code for this variant.
    ///
    /// These codes are the canonical extension-error v2 codes and match the
    /// variant names in `greentic:extension-base/types@0.2.0::extension-error`.
    /// Downstream consumers (greentic-start, FlowOutcome surfacing) can switch
    /// on these without depending on this crate's concrete enum.
    pub const fn code(&self) -> &'static str {
        match self {
            ProviderInvokeError::InvalidInput(_) => "invalid-input",
            ProviderInvokeError::MissingCapability(_) => "missing-capability",
            ProviderInvokeError::PermissionDenied(_) => "permission-denied",
            ProviderInvokeError::NotFound(_) => "not-found",
            ProviderInvokeError::SchemaInvalid(_) => "schema-invalid",
            ProviderInvokeError::Internal(_) => "internal",
        }
    }
}

/// Map a `@0.2.0` `extension-error` (six variants) into [`ProviderInvokeError`].
///
/// Lossless: the two share the same six variants.
pub fn map_v2(err: v0_2_0::greentic::extension_base::types::ExtensionError) -> ProviderInvokeError {
    use v0_2_0::greentic::extension_base::types::ExtensionError as E;
    match err {
        E::InvalidInput(s) => ProviderInvokeError::InvalidInput(s),
        E::MissingCapability(s) => ProviderInvokeError::MissingCapability(s),
        E::PermissionDenied(s) => ProviderInvokeError::PermissionDenied(s),
        E::NotFound(s) => ProviderInvokeError::NotFound(s),
        E::SchemaInvalid(s) => ProviderInvokeError::SchemaInvalid(s),
        E::Internal(s) => ProviderInvokeError::Internal(s),
    }
}

/// Map a `@0.1.0` provider `error` (three variants) into
/// [`ProviderInvokeError`].
///
/// The `@0.1.0` provider interfaces carry their own three-variant `error`
/// (`not-found`, `schema-invalid`, `internal`) rather than the base
/// `extension-error`. Each maps losslessly onto the corresponding v2 variant;
/// no `@0.1.0` value can produce `invalid-input`, `missing-capability`, or
/// `permission-denied`.
pub fn map_v1(err: v0_1_0::greentic::extension_provider::types::Error) -> ProviderInvokeError {
    use v0_1_0::greentic::extension_provider::types::Error as E;
    match err {
        E::NotFound(s) => ProviderInvokeError::NotFound(s),
        E::SchemaInvalid(s) => ProviderInvokeError::SchemaInvalid(s),
        E::Internal(s) => ProviderInvokeError::Internal(s),
    }
}

/// Map a legacy schema-core failure (which has no typed error channel — the
/// WIT returns raw bytes and surfaces failures as host/anyhow strings) into
/// the typed error space as `internal`.
///
/// Used so callers that adopt [`ProviderInvokeError`] can treat legacy and
/// extension-provider failures uniformly: legacy errors collapse to
/// `internal` with the original message preserved.
pub fn map_legacy(message: impl Into<String>) -> ProviderInvokeError {
    ProviderInvokeError::Internal(message.into())
}

/// Attach a [`ProviderInvokeError`] to an `anyhow` error chain so downstream
/// can `downcast_ref::<ProviderInvokeError>()` while the `Display` chain still
/// carries the kebab `code` for log/telemetry surfaces.
///
/// This mirrors the pattern greentic-designer uses to thread typed extension
/// errors through an `anyhow::Result` boundary without changing the public
/// signature: the typed error is the *source*, and a context line prefixes the
/// stable code.
pub fn into_anyhow(err: ProviderInvokeError) -> anyhow::Error {
    let code = err.code();
    anyhow::Error::new(err).context(format!("provider error [{code}]"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worlds_bind_to_distinct_types() {
        // The two generations must produce distinct Rust types; if bindgen
        // collapsed them we would have a silent ABI mismatch.
        let v2 = std::any::type_name::<v0_2_0::FullProviderPre<()>>();
        let v1 = std::any::type_name::<v0_1_0::FullProviderPre<()>>();
        assert_ne!(v2, v1);
    }

    #[test]
    fn provider_world_version_strings() {
        assert_eq!(ProviderWorld::V0_2_0.version(), "0.2.0");
        assert_eq!(ProviderWorld::V0_1_0.version(), "0.1.0");
    }

    #[test]
    fn v2_error_maps_all_six_variants_losslessly() {
        use v0_2_0::greentic::extension_base::types::ExtensionError as E;
        let cases = [
            (E::InvalidInput("a".into()), "invalid-input"),
            (E::MissingCapability("b".into()), "missing-capability"),
            (E::PermissionDenied("c".into()), "permission-denied"),
            (E::NotFound("d".into()), "not-found"),
            (E::SchemaInvalid("e".into()), "schema-invalid"),
            (E::Internal("f".into()), "internal"),
        ];
        for (wit, expected_code) in cases {
            let mapped = map_v2(wit);
            assert_eq!(mapped.code(), expected_code);
        }
    }

    #[test]
    fn v2_error_preserves_message() {
        use v0_2_0::greentic::extension_base::types::ExtensionError as E;
        let mapped = map_v2(E::InvalidInput("bad payload".into()));
        assert_eq!(
            mapped,
            ProviderInvokeError::InvalidInput("bad payload".into())
        );
        assert_eq!(mapped.to_string(), "invalid input: bad payload");
    }

    #[test]
    fn v1_error_maps_three_variants_losslessly() {
        use v0_1_0::greentic::extension_provider::types::Error as E;
        let cases = [
            (E::NotFound("x".into()), "not-found"),
            (E::SchemaInvalid("y".into()), "schema-invalid"),
            (E::Internal("z".into()), "internal"),
        ];
        for (wit, expected_code) in cases {
            let mapped = map_v1(wit);
            assert_eq!(mapped.code(), expected_code);
        }
    }

    #[test]
    fn v1_error_preserves_message() {
        use v0_1_0::greentic::extension_provider::types::Error as E;
        let mapped = map_v1(E::NotFound("no such channel".into()));
        assert_eq!(
            mapped,
            ProviderInvokeError::NotFound("no such channel".into())
        );
    }

    #[test]
    fn legacy_maps_to_internal() {
        let mapped = map_legacy("boom");
        assert_eq!(mapped, ProviderInvokeError::Internal("boom".into()));
        assert_eq!(mapped.code(), "internal");
    }

    #[test]
    fn all_codes_are_canonical_kebab() {
        let all = [
            ProviderInvokeError::InvalidInput(String::new()),
            ProviderInvokeError::MissingCapability(String::new()),
            ProviderInvokeError::PermissionDenied(String::new()),
            ProviderInvokeError::NotFound(String::new()),
            ProviderInvokeError::SchemaInvalid(String::new()),
            ProviderInvokeError::Internal(String::new()),
        ];
        let codes: Vec<_> = all.iter().map(|e| e.code()).collect();
        assert_eq!(
            codes,
            vec![
                "invalid-input",
                "missing-capability",
                "permission-denied",
                "not-found",
                "schema-invalid",
                "internal",
            ]
        );
    }

    #[test]
    fn into_anyhow_preserves_downcast_and_code_in_display() {
        let typed = ProviderInvokeError::PermissionDenied("nope".into());
        let err = into_anyhow(typed.clone());
        // Code surfaces in the Display chain for logs/telemetry.
        assert!(err.to_string().contains("permission-denied"));
        // Downstream can still recover the typed error.
        let recovered = err
            .downcast_ref::<ProviderInvokeError>()
            .expect("typed error preserved in anyhow chain");
        assert_eq!(recovered, &typed);
        assert_eq!(recovered.code(), "permission-denied");
    }
}
