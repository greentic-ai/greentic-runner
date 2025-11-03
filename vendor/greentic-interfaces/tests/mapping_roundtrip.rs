use std::convert::TryFrom;

use greentic_interfaces::bindings;
use greentic_types as types;
use semver::Version;
use time::OffsetDateTime;

fn sample_tenant_ctx() -> types::TenantCtx {
    types::TenantCtx {
        env: types::EnvId::from("dev"),
        tenant: types::TenantId::from("tenant"),
        tenant_id: types::TenantId::from("tenant"),
        team: Some(types::TeamId::from("team")),
        team_id: Some(types::TeamId::from("team")),
        user: Some(types::UserId::from("user")),
        user_id: Some(types::UserId::from("user")),
        trace_id: Some("trace".into()),
        correlation_id: Some("corr".into()),
        deadline: Some(types::InvocationDeadline::from_unix_millis(
            1_700_000_000_000,
        )),
        attempt: 1,
        idempotency_key: Some("idem".into()),
        impersonation: Some(types::Impersonation {
            actor_id: types::UserId::from("actor"),
            reason: Some("maintenance".into()),
        }),
    }
}

#[test]
fn tenant_ctx_roundtrip_external() {
    let ctx = sample_tenant_ctx();
    let wit = match bindings::greentic::interfaces_types::types::TenantCtx::try_from(ctx.clone()) {
        Ok(value) => value,
        Err(err) => panic!("wit conversion failed: {err}"),
    };
    let round = match types::TenantCtx::try_from(wit) {
        Ok(value) => value,
        Err(err) => panic!("rust conversion failed: {err}"),
    };
    assert_eq!(round.env.as_str(), ctx.env.as_str());
    assert_eq!(round.attempt, ctx.attempt);
    assert_eq!(
        round
            .impersonation
            .as_ref()
            .map(|imp| imp.actor_id.as_str()),
        ctx.impersonation.as_ref().map(|imp| imp.actor_id.as_str())
    );
}

#[test]
fn outcome_roundtrip_external() {
    use bindings::greentic::interfaces_types::types::Outcome as WitOutcome;

    let outcome = types::Outcome::Pending {
        reason: "waiting".into(),
        expected_input: Some(vec!["input".into()]),
    };
    let wit: WitOutcome = outcome.clone().into();
    let round: types::Outcome<String> = wit.into();
    match round {
        types::Outcome::Pending {
            reason,
            expected_input,
        } => {
            assert_eq!(reason, "waiting");
            assert_eq!(
                expected_input.unwrap_or_default(),
                vec!["input".to_string()]
            );
        }
        _ => panic!("expected pending"),
    }
}

#[test]
fn span_context_roundtrip() {
    use bindings::greentic::interfaces_types::types::SpanContext as WitSpanContext;

    let span = types::SpanContext {
        tenant: types::TenantId::from("tenant"),
        session_id: Some(types::SessionKey::from("session")),
        flow_id: "flow".into(),
        node_id: Some("node".into()),
        provider: "provider".into(),
        start: Some(OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()),
        end: None,
    };
    let wit = WitSpanContext::try_from(span.clone()).expect("to wit");
    let round = types::SpanContext::try_from(wit).expect("from wit");
    assert_eq!(round.provider, span.provider);
    assert_eq!(round.node_id, span.node_id);
}

#[test]
fn allow_list_roundtrip() {
    use bindings::greentic::interfaces_types::types::AllowList as WitAllowList;

    let list = types::AllowList {
        domains: vec!["example.com".into()],
        ports: vec![443],
        protocols: vec![
            types::Protocol::Https,
            types::Protocol::Custom("mqtt".into()),
        ],
    };
    let wit: WitAllowList = list.clone().into();
    let round: types::AllowList = wit.into();
    assert_eq!(round.domains, list.domains);
    assert_eq!(round.ports, list.ports);
    assert_eq!(round.protocols.len(), list.protocols.len());
}

#[test]
fn pack_ref_roundtrip() {
    use bindings::greentic::interfaces_types::types::PackRef as WitPackRef;

    let pack = types::PackRef {
        oci_url: "registry.example.com/pack".into(),
        version: Version::parse("1.2.3").expect("valid version"),
        digest: "sha256:deadbeef".into(),
        signatures: vec![types::Signature {
            key_id: "key1".into(),
            algorithm: types::SignatureAlgorithm::Ed25519,
            signature: vec![1, 2, 3],
        }],
    };
    let wit = WitPackRef::from(pack.clone());
    let round = types::PackRef::try_from(wit).expect("from wit");
    assert_eq!(round.oci_url, pack.oci_url);
    assert_eq!(round.version, pack.version);
    assert_eq!(round.digest, pack.digest);
    assert_eq!(round.signatures.len(), pack.signatures.len());
}
