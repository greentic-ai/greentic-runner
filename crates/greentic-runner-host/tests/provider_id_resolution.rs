//! M1.1b regression tests for ProviderRegistry post-cutover.
//!
//! Verifies that `extract_inline_providers` preserves the wire
//! `provider_id` (Option<String>) verbatim instead of synthesizing
//! `Some(provider_type)`, and that `ProviderRegistry::resolve` enforces
//! defense-in-depth when a caller supplies both id and type.

use std::collections::BTreeMap;

use anyhow::Result;
use greentic_runner_host::provider::ProviderRegistry;
use greentic_types::{
    ExtensionInline, ExtensionRef, PROVIDER_EXTENSION_ID, PackId, PackKind, PackManifest,
    PackSignatures, ProviderDecl, ProviderExtensionInline, ProviderRuntimeRef,
};
use semver::Version;

fn decl(provider_type: &str, provider_id: Option<&str>) -> ProviderDecl {
    ProviderDecl {
        provider_type: provider_type.into(),
        provider_id: provider_id.map(str::to_owned),
        capabilities: Vec::new(),
        ops: Vec::new(),
        config_schema_ref: "schemas/x.json".into(),
        state_schema_ref: None,
        runtime: ProviderRuntimeRef {
            component_ref: format!("{provider_type}.runtime"),
            export: "provider-core".into(),
            world: "greentic:provider-core@1.0.0".into(),
        },
        docs_ref: None,
    }
}

fn manifest_with(providers: Vec<ProviderDecl>) -> Result<PackManifest> {
    let inline = ProviderExtensionInline {
        providers,
        ..Default::default()
    };
    let mut extensions = BTreeMap::new();
    extensions.insert(
        PROVIDER_EXTENSION_ID.to_string(),
        ExtensionRef {
            kind: PROVIDER_EXTENSION_ID.to_string(),
            version: "1.0.0".into(),
            digest: None,
            location: None,
            inline: Some(ExtensionInline::Provider(inline)),
        },
    );
    Ok(PackManifest {
        schema_version: "1.0".into(),
        pack_id: PackId::new("vendor.providers")?,
        name: None,
        version: Version::parse("0.1.0")?,
        kind: PackKind::Application,
        publisher: "test".into(),
        components: Vec::new(),
        flows: Vec::new(),
        dependencies: Vec::new(),
        capabilities: Vec::new(),
        secret_requirements: Vec::new(),
        signatures: PackSignatures::default(),
        bootstrap: None,
        extensions: Some(extensions),
    })
}

fn registry_for(providers: Vec<ProviderDecl>) -> Result<ProviderRegistry> {
    let manifest = manifest_with(providers)?;
    ProviderRegistry::new(&manifest, None, "demo", "local")
}

#[test]
fn resolve_by_provider_id_returns_inline_decl_with_matching_id() {
    let registry = registry_for(vec![decl("vendor.cache", Some("vendor.cache.primary"))]).unwrap();
    let binding = registry
        .resolve(Some("vendor.cache.primary"), None)
        .expect("inline decl with provider_id resolves by id");
    assert_eq!(binding.provider_id.as_deref(), Some("vendor.cache.primary"));
    assert_eq!(binding.provider_type, "vendor.cache");
}

#[test]
fn resolve_by_id_no_longer_matches_provider_type_post_cutover() {
    // Pre-M1.1b, extract_inline_providers synthesized `provider_id =
    // Some(provider_type)`, so `resolve(Some("vendor.search"), None)` against
    // a decl with no `provider_id` would silently succeed by id-fallback.
    // Post-cutover this MUST NOT resolve via the id path.
    let registry = registry_for(vec![decl("vendor.search", None)]).unwrap();
    let err = registry
        .resolve(Some("vendor.search"), None)
        .expect_err("no inline decl declares provider_id 'vendor.search'");
    assert!(
        err.to_string()
            .contains("provider_id `vendor.search` not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn resolve_by_provider_type_still_works_for_unnamed_decl() {
    let registry = registry_for(vec![decl("vendor.search", None)]).unwrap();
    let binding = registry
        .resolve(None, Some("vendor.search"))
        .expect("provider_type lookup still resolves unnamed decls");
    assert_eq!(binding.provider_type, "vendor.search");
    assert_eq!(binding.provider_id.as_deref(), Some("vendor.search"));
}

#[test]
fn resolve_rejects_id_type_mismatch_when_both_supplied() {
    // Defense-in-depth: caller supplies both provider_id and provider_type.
    // The inline decl carries provider_id="shared" with provider_type="vendor.a".
    // A caller asking for provider_type="vendor.b" must fail loudly, not get
    // an unexpected vendor.a binding.
    let registry = registry_for(vec![decl("vendor.a", Some("shared"))]).unwrap();
    let err = registry
        .resolve(Some("shared"), Some("vendor.b"))
        .expect_err("id/type mismatch must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("provider_id `shared`")
            && msg.contains("`vendor.a`")
            && msg.contains("`vendor.b`"),
        "expected mismatch diagnostic naming both types, got: {msg}"
    );
}

#[test]
fn resolve_with_matching_id_and_type_succeeds() {
    let registry = registry_for(vec![decl("vendor.cache", Some("vendor.cache.primary"))]).unwrap();
    let binding = registry
        .resolve(Some("vendor.cache.primary"), Some("vendor.cache"))
        .expect("matching id + type pair resolves");
    assert_eq!(binding.provider_id.as_deref(), Some("vendor.cache.primary"));
    assert_eq!(binding.provider_type, "vendor.cache");
}
