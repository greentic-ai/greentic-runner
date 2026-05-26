//! PR-1: `TenantRuntime::load_revision` + `ActivePacks::insert_revision`.
//!
//! Proves the revision-runtime producer seam end to end against a real fixture
//! pack: a runtime builds from an explicit pack list, carries the rollout
//! identity on its flow engine (the C5 telemetry producer), and round-trips
//! through the revision-keyed `ActivePacks` map without disturbing the legacy
//! tenant entry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use greentic_deploy_spec::ids::{BundleId, DeploymentId, RevisionId};
use greentic_runner_host::RunnerWasiPolicy;
use greentic_runner_host::config::{
    FlowRetryConfig, HostConfig, OperatorPolicy, RateLimits, SecretsPolicy, StateStorePolicy,
    WebhookPolicy,
};
use greentic_runner_host::runtime::{ActivePacks, TenantRuntime};
use greentic_runner_host::secrets::default_manager;
use greentic_runner_host::storage::{
    new_session_store, new_state_store, session_host_from, state_host_from,
};
use greentic_runner_host::telemetry::RolloutIds;
use greentic_runner_host::trace::TraceConfig;
use greentic_runner_host::validate::ValidationConfig;

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

fn host_config(bindings_path: &Path) -> Arc<HostConfig> {
    Arc::new(HostConfig {
        tenant: "acme".into(),
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

async fn build_revision(rollout: RolloutIds) -> Result<Arc<TenantRuntime>> {
    let pack = fixture_pack();
    let config = host_config(&pack);
    let session_store = new_session_store();
    let session_host = session_host_from(Arc::clone(&session_store));
    let state_store = new_state_store();
    let state_host = state_host_from(Arc::clone(&state_store));
    let manager = default_manager().context("default manager")?;
    TenantRuntime::load_revision(
        std::slice::from_ref(&pack),
        config,
        None,
        Arc::new(RunnerWasiPolicy::new()),
        session_host,
        session_store,
        state_store,
        state_host,
        manager,
        rollout,
    )
    .await
}

#[tokio::test]
async fn load_revision_builds_and_stamps_rollout_identity() -> Result<()> {
    let rollout = RolloutIds {
        customer_id: Some("cust-acme".into()),
        deployment_id: Some("01JTKSDEP".into()),
        bundle_id: Some("customer.support".into()),
        revision_id: Some("01JTKSREV".into()),
    };
    let runtime = build_revision(rollout.clone()).await?;
    // The producer wired the rollout identity onto the flow engine, so every
    // span this runtime emits will carry deployment/bundle/revision attribution.
    assert_eq!(runtime.engine().rollout_ids(), &rollout);
    Ok(())
}

#[tokio::test]
async fn insert_revision_round_trips_and_preserves_legacy() -> Result<()> {
    let active = ActivePacks::new();

    // A pre-existing legacy (tenant-only) runtime must survive the insert.
    let legacy = build_revision(RolloutIds::default()).await?;
    active.insert_pack("acme", legacy);

    let deployment = DeploymentId::new();
    let bundle = BundleId::from("customer.support");
    let revision = RevisionId::new();
    let runtime = build_revision(RolloutIds {
        revision_id: Some(revision.to_string()),
        ..Default::default()
    })
    .await?;
    active.insert_revision("acme", deployment, bundle.clone(), revision, runtime);

    assert!(
        active
            .load_revision("acme", deployment, bundle, revision)
            .is_some(),
        "revision runtime must be retrievable by its full key"
    );
    assert!(
        active.load_pack("acme").is_some(),
        "legacy tenant runtime must survive a revision insert"
    );
    Ok(())
}
