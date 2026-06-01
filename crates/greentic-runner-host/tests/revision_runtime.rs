//! PR-1: `TenantRuntime::load_revision` + `ActivePacks::insert_revision`.
//!
//! Proves the revision-runtime producer seam end to end against a real fixture
//! pack: a runtime builds from a digest-pinned pack list, fails closed on a
//! digest mismatch, carries the rollout identity derived from its typed key
//! (the C5 telemetry producer), and round-trips through the revision-keyed
//! `ActivePacks` map — which rejects any runtime whose tenant or rollout
//! identity does not match the key it would be stored under.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use greentic_deploy_spec::ids::{BundleId, DeploymentId, RevisionId};
use greentic_runner_host::config::{
    FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
    WebhookPolicy,
};
use greentic_runner_host::runtime::{ActivePacks, RevisionPackRef, TenantRuntime};
use greentic_runner_host::secrets::default_manager;
use greentic_runner_host::storage::{
    new_session_store, new_state_store, session_host_from, state_host_from,
};
use greentic_runner_host::telemetry::RolloutIds;
use greentic_runner_host::trace::TraceConfig;
use greentic_runner_host::validate::ValidationConfig;
use greentic_runner_host::{Activity, HostBuilder, RunnerWasiPolicy};
use runner_core::packs::PackDigest;

const TENANT: &str = "acme";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .expect("workspace root")
}

fn fixture_pack() -> PathBuf {
    workspace_root().join("tests/fixtures/packs/runner-components/runner-components.gtpack")
}

/// One digest-pinned ref over the fixture pack, with the real sha256 so the
/// integrity check passes.
fn pinned_pack_refs() -> Result<Vec<RevisionPackRef>> {
    let path = fixture_pack();
    let bytes = std::fs::read(&path).context("read fixture pack")?;
    let digest = PackDigest::sha256_from_bytes(&bytes).raw_string();
    Ok(vec![RevisionPackRef { path, digest }])
}

fn host_config(bindings_path: &Path) -> Arc<HostConfig> {
    Arc::new(HostConfig {
        tenant: TENANT.into(),
        bindings_path: bindings_path.to_path_buf(),
        flow_type_bindings: Default::default(),
        rate_limits: RateLimits::default(),
        retry: FlowRetryConfig::default(),
        http_enabled: false,
        secrets_policy: SecretsPolicy::allow_all(),
        state_store_policy: StateStorePolicy::default(),
        webhook_policy: WebhookPolicy::default(),
        timers: Vec::new(),
        oauth: None,
        mocks: None,
        pack_bindings: Vec::new(),
        env_passthrough: Vec::new(),
        trace: TraceConfig::from_env(),
        validation: ValidationConfig::from_env(),
        operator_policy: OperatorPolicy::allow_all(),
    })
}

async fn build_revision(
    pack_refs: &[RevisionPackRef],
    deployment_id: DeploymentId,
    bundle_id: BundleId,
    revision_id: RevisionId,
    customer_id: Option<String>,
) -> Result<Arc<TenantRuntime>> {
    let config = host_config(&fixture_pack());
    let session_store = new_session_store();
    let session_host = session_host_from(Arc::clone(&session_store));
    let state_store = new_state_store();
    let state_host = state_host_from(Arc::clone(&state_store));
    let manager = default_manager().context("default manager")?;
    TenantRuntime::load_revision(
        pack_refs,
        config,
        None,
        Arc::new(RunnerWasiPolicy::new()),
        session_host,
        session_store,
        state_store,
        state_host,
        manager,
        deployment_id,
        bundle_id,
        revision_id,
        customer_id,
    )
    .await
}

/// A true legacy (tenant-only) runtime: built via `TenantRuntime::load` with the
/// default (empty) rollout identity, matching how the pack watcher constructs
/// tenant runtimes in production.
async fn build_legacy() -> Result<Arc<TenantRuntime>> {
    let pack = fixture_pack();
    let config = host_config(&pack);
    let session_store = new_session_store();
    let session_host = session_host_from(Arc::clone(&session_store));
    let state_store = new_state_store();
    let state_host = state_host_from(Arc::clone(&state_store));
    let manager = default_manager().context("default manager")?;
    TenantRuntime::load(
        &pack,
        config,
        None,
        Some(pack.as_path()),
        None,
        Arc::new(RunnerWasiPolicy::new()),
        session_host,
        session_store,
        state_store,
        state_host,
        manager,
    )
    .await
}

#[tokio::test]
async fn load_revision_derives_rollout_identity_and_records_digests() -> Result<()> {
    let refs = pinned_pack_refs()?;
    let deployment = DeploymentId::new();
    let revision = RevisionId::new();
    let runtime = build_revision(
        &refs,
        deployment,
        BundleId::from("customer.support"),
        revision,
        Some("cust-acme".into()),
    )
    .await?;

    // Rollout identity is derived from the typed key, so the engine's telemetry
    // attribution matches the revision the runtime serves.
    let expected = RolloutIds {
        customer_id: Some("cust-acme".into()),
        deployment_id: Some(deployment.to_string()),
        bundle_id: Some("customer.support".into()),
        revision_id: Some(revision.to_string()),
    };
    assert_eq!(runtime.engine().rollout_ids(), &expected);

    // The verified digest is threaded into the runtime (parity with the legacy
    // index path), not dropped to `None`.
    assert_eq!(runtime.pack_digests(), &[Some(refs[0].digest.clone())]);
    Ok(())
}

#[tokio::test]
async fn load_revision_rejects_digest_mismatch() -> Result<()> {
    let tampered = vec![RevisionPackRef {
        path: fixture_pack(),
        // Well-formed `algo:value` but not the pack's real content digest.
        digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
    }];
    let Err(err) = build_revision(
        &tampered,
        DeploymentId::new(),
        BundleId::from("customer.support"),
        RevisionId::new(),
        None,
    )
    .await
    else {
        panic!("digest mismatch must fail closed");
    };
    assert!(
        format!("{err:#}").contains("does not match pinned digest"),
        "{err:#}"
    );
    Ok(())
}

#[tokio::test]
async fn load_revision_rejects_unsupported_digest_algorithm() -> Result<()> {
    // Well-formed `algo:value`, but an algorithm the verifier can never produce.
    let wrong_algo = vec![RevisionPackRef {
        path: fixture_pack(),
        digest: "sha512:abc".into(),
    }];
    let Err(err) = build_revision(
        &wrong_algo,
        DeploymentId::new(),
        BundleId::from("customer.support"),
        RevisionId::new(),
        None,
    )
    .await
    else {
        panic!("unsupported digest algorithm must be rejected");
    };
    assert!(
        format!("{err:#}").contains("unsupported digest algorithm"),
        "{err:#}"
    );
    Ok(())
}

#[tokio::test]
async fn insert_revision_round_trips_and_preserves_legacy() -> Result<()> {
    let active = ActivePacks::new();
    let deployment = DeploymentId::new();
    let bundle = BundleId::from("customer.support");
    let revision = RevisionId::new();

    // A pre-existing legacy (tenant-only) runtime — real production shape, with
    // no rollout identity — must survive the revision insert.
    let legacy = build_legacy().await?;
    assert!(
        legacy.engine().rollout_ids().is_empty(),
        "legacy runtime must carry no rollout identity"
    );
    active.insert_pack(TENANT, legacy);

    let runtime = build_revision(
        &pinned_pack_refs()?,
        deployment,
        bundle.clone(),
        revision,
        None,
    )
    .await?;
    active.insert_revision(TENANT, deployment, bundle.clone(), revision, runtime)?;

    assert!(
        active
            .load_revision(TENANT, deployment, bundle, revision)
            .is_some(),
        "revision runtime must be retrievable by its full key"
    );
    assert!(
        active.load_pack(TENANT).is_some(),
        "legacy tenant runtime must survive a revision insert"
    );
    Ok(())
}

#[tokio::test]
async fn insert_revision_rejects_tenant_mismatch() -> Result<()> {
    let active = ActivePacks::new();
    let deployment = DeploymentId::new();
    let bundle = BundleId::from("customer.support");
    let revision = RevisionId::new();
    // Runtime is built for tenant `acme` (from host_config).
    let runtime = build_revision(
        &pinned_pack_refs()?,
        deployment,
        bundle.clone(),
        revision,
        None,
    )
    .await?;

    let Err(err) = active.insert_revision("other-tenant", deployment, bundle, revision, runtime)
    else {
        panic!("tenant mismatch must be rejected");
    };
    assert!(
        format!("{err:#}").contains("does not match key tenant"),
        "{err:#}"
    );
    Ok(())
}

/// Build a minimal multi-tenant host. Revision execution looks the runtime up
/// in `active_packs()` and runs *that runtime's* own state machine, so the
/// host's stores are not on the execution path — it only needs the tenant
/// registered so `HostBuilder::build` accepts it.
fn build_host() -> Result<greentic_runner_host::RunnerHost> {
    let config = (*host_config(&fixture_pack())).clone();
    HostBuilder::new().with_config(config).build()
}

#[tokio::test]
async fn handle_activity_for_revision_dispatches_like_the_legacy_path() -> Result<()> {
    // Both entry points funnel through `dispatch_activity`, so a revision-keyed
    // runtime must execute identically to the same pack loaded as a legacy
    // tenant runtime. We assert parity with the legacy path (already proven to
    // emit replies in `host_integration::host_executes_demo_pack_flow`) rather
    // than a fixed reply, so the test tracks the shared dispatch contract — the
    // one thing this method adds, the revision lookup — not fixture-flow output.
    let host = build_host()?;
    let deployment = DeploymentId::new();
    let bundle = BundleId::from("customer.support");
    let revision = RevisionId::new();

    // The same fixture pack, loaded both ways: legacy tenant-only and
    // revision-keyed, both retrievable from the one host's `ActivePacks`.
    host.active_packs()
        .insert_pack(TENANT, build_legacy().await?);
    let rev_runtime = build_revision(
        &pinned_pack_refs()?,
        deployment,
        bundle.clone(),
        revision,
        None,
    )
    .await?;
    host.active_packs().insert_revision(
        TENANT,
        deployment,
        bundle.clone(),
        revision,
        rev_runtime,
    )?;

    let activity = || {
        Activity::text("hello")
            .with_tenant(TENANT)
            .from_user("user-1")
    };
    let legacy = host.handle_activity(TENANT, activity()).await;
    let revisioned = host
        .handle_activity_for_revision(TENANT, deployment, bundle, revision, activity())
        .await;

    assert_eq!(
        legacy.is_ok(),
        revisioned.is_ok(),
        "revision dispatch must match the legacy path: legacy={legacy:?} revision={revisioned:?}"
    );
    match (legacy, revisioned) {
        (Ok(l), Ok(r)) => assert_eq!(
            l.is_empty(),
            r.is_empty(),
            "revision replies must match legacy reply shape"
        ),
        (Err(l), Err(r)) => assert_eq!(
            format!("{l:#}"),
            format!("{r:#}"),
            "any divergence must be in the shared flow path, not the revision lookup"
        ),
        _ => unreachable!("is_ok parity asserted above"),
    }
    Ok(())
}

#[tokio::test]
async fn handle_activity_for_revision_errors_when_not_loaded() -> Result<()> {
    // A host with a registered tenant but no revision inserted: the revision
    // lookup must fail closed rather than fall back to the legacy entry.
    let host = build_host()?;
    let activity = Activity::text("nobody home").with_tenant(TENANT);
    let Err(err) = host
        .handle_activity_for_revision(
            TENANT,
            DeploymentId::new(),
            BundleId::from("customer.support"),
            RevisionId::new(),
            activity,
        )
        .await
    else {
        panic!("an unloaded revision must not execute");
    };
    assert!(
        format!("{err:#}").contains("revision runtime not loaded"),
        "{err:#}"
    );
    Ok(())
}

#[tokio::test]
async fn insert_revision_rejects_revision_identity_mismatch() -> Result<()> {
    let active = ActivePacks::new();
    let deployment = DeploymentId::new();
    let bundle = BundleId::from("customer.support");
    let built_revision = RevisionId::new();
    // Runtime carries `built_revision` in its derived rollout identity.
    let runtime = build_revision(
        &pinned_pack_refs()?,
        deployment,
        bundle.clone(),
        built_revision,
        None,
    )
    .await?;

    // Insert under a different revision id — the rollout identity no longer
    // matches the key.
    let other_revision = RevisionId::new();
    let Err(err) = active.insert_revision(TENANT, deployment, bundle, other_revision, runtime)
    else {
        panic!("revision identity mismatch must be rejected");
    };
    assert!(format!("{err:#}").contains("rollout identity"), "{err:#}");
    Ok(())
}
