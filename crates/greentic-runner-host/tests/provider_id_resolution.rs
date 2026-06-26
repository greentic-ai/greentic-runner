//! Provider resolution regression tests.
//!
//! Post greentic-types 1.1.0-dev.27836473437 the `ProviderDecl.provider_id`
//! field was removed. Inline manifest providers are now identified solely by
//! `provider_type`. These tests verify that `ProviderRegistry::resolve` works
//! correctly under the new schema.

use std::collections::BTreeMap;

use anyhow::Result;
use greentic_runner_host::provider::ProviderRegistry;
use greentic_types::{
    ExtensionInline, ExtensionRef, PROVIDER_EXTENSION_ID, PackId, PackKind, PackManifest,
    PackSignatures, ProviderDecl, ProviderExtensionInline, ProviderRuntimeRef,
};
use semver::Version;

fn decl(provider_type: &str) -> ProviderDecl {
    ProviderDecl {
        provider_type: provider_type.into(),
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
        agents: Default::default(),
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
fn resolve_by_provider_type_works() {
    let registry = registry_for(vec![decl("vendor.cache")]).unwrap();
    let binding = registry
        .resolve(None, Some("vendor.cache"))
        .expect("provider_type lookup resolves inline decl");
    assert_eq!(binding.provider_type, "vendor.cache");
}

#[test]
fn resolve_by_id_fails_for_inline_provider_without_instance_file() {
    // Inline providers no longer carry a provider_id (field removed from
    // ProviderDecl). Resolving by id against an inline-only registry must
    // fail — only instance-file-loaded providers have ids.
    let registry = registry_for(vec![decl("vendor.search")]).unwrap();
    let err = registry
        .resolve(Some("vendor.search"), None)
        .expect_err("inline decl has no provider_id, id-lookup must fail");
    assert!(
        err.to_string()
            .contains("provider_id `vendor.search` not found"),
        "unexpected error: {err}"
    );
}

#[test]
fn resolve_by_provider_type_fills_default_provider_id() {
    // When resolving by type alone the binding's provider_id defaults to the
    // provider_type string (see binding_from_decl's default_provider_id arg).
    let registry = registry_for(vec![decl("vendor.search")]).unwrap();
    let binding = registry
        .resolve(None, Some("vendor.search"))
        .expect("provider_type lookup resolves");
    assert_eq!(binding.provider_type, "vendor.search");
    assert_eq!(binding.provider_id.as_deref(), Some("vendor.search"));
}

#[test]
fn resolve_rejects_unknown_provider_type() {
    let registry = registry_for(vec![decl("vendor.cache")]).unwrap();
    let err = registry
        .resolve(None, Some("vendor.unknown"))
        .expect_err("unknown provider_type must fail");
    assert!(
        err.to_string().contains("no provider runtime found"),
        "unexpected error: {err}"
    );
}

#[test]
fn resolve_rejects_ambiguous_multiple_providers_of_same_type() {
    let err = registry_for(vec![decl("vendor.cache"), decl("vendor.cache")])
        .err()
        .expect("duplicate provider_type rejected at registry construction");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("duplicate provider_type"),
        "expected duplicate provider_type validation failure, got: {msg}"
    );
}
