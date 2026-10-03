use aws_lc_rs::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair as _},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use managed_inference_contract::*;
use std::collections::BTreeMap;

fn fixture_claims() -> ManagedInferenceAuthorizationClaims {
    ManagedInferenceAuthorizationClaims {
        spend_authority: None,
        schema_version: AUTHORIZATION_SCHEMA_VERSION,
        authorization_id: "auth-1".into(),
        key_id: "maestro-2026-08".into(),
        audience: AUTHORIZATION_AUDIENCE.into(),
        issued_at_ms: 1_700_000_000_000,
        not_before_ms: 1_700_000_000_000,
        expires_at_ms: 1_700_000_060_000,
        grant_epoch: 7,
        organization_id: "org-1".into(),
        workspace_id: "ws-1".into(),
        session_id: "session-1".into(),
        thread_id: "thread-1".into(),
        run_id: "run-1".into(),
        turn_id: "turn-1".into(),
        lineage_id: "lineage-1".into(),
        runtime_generation: 42,
        endpoint: "responses".into(),
        model: "gpt-5.6".into(),
        provider_candidates: vec![
            ManagedInferenceProviderCandidate {
                provider: "openai".into(),
                environment: "production".into(),
                credential_name: "maestro".into(),
                team_id: "team-1".into(),
                model: "gpt-5.6".into(),
            },
            ManagedInferenceProviderCandidate {
                provider: "anthropic".into(),
                environment: "production".into(),
                credential_name: "maestro".into(),
                team_id: "team-1".into(),
                model: "claude-sonnet".into(),
            },
        ],
        routing: ManagedInferenceRouting::Ordered,
        output_token_budget: ManagedInferenceOutputTokenBudget {
            value: Some(4096),
            origin: ManagedInferenceBudgetOrigin::RoutePolicy,
            origin_reference: "policy-1".into(),
        },
        scope_digest: String::new(),
    }
}

fn fixture_scope() -> ManagedInferenceScope {
    let claims = fixture_claims();
    ManagedInferenceScope {
        organization_id: "org-1".into(),
        workspace_id: "ws-1".into(),
        session_id: "session-1".into(),
        thread_id: "thread-1".into(),
        run_id: "run-1".into(),
        turn_id: "turn-1".into(),
        lineage_id: "lineage-1".into(),
        endpoint: "responses".into(),
        model: "gpt-5.6".into(),
        provider_candidates: claims.provider_candidates,
        routing: ManagedInferenceRouting::Ordered,
        output_token_budget: claims.output_token_budget,
    }
}

fn signed_fixture() -> (ManagedInferenceAuthorization, Vec<u8>) {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
    let mut claims = fixture_claims();
    claims.scope_digest = scope_digest(&fixture_scope()).unwrap();
    let authorization = sign_authorization(&claims, document.as_ref()).unwrap();
    (authorization, pair.public_key().as_ref().to_vec())
}

fn verification_context(keys: &BTreeMap<String, Vec<u8>>) -> AuthorizationVerificationContext {
    AuthorizationVerificationContext {
        expected_scope: fixture_scope(),
        expected_audience: AUTHORIZATION_AUDIENCE.into(),
        current_grant_epoch: 7,
        now_ms: 1_700_000_030_000,
        clock_skew_ms: 60_000,
        keys: ManagedInferenceKeySet::from_raw(keys.clone()).unwrap(),
    }
}
#[test]
fn authorization_rejects_tampered_claim_audience_expiry_epoch_and_signature() {
    let (authorization, public_key) = signed_fixture();
    let mut keys = BTreeMap::new();
    keys.insert("maestro-2026-08".into(), public_key);

    for mutate in [
        |claims: &mut ManagedInferenceAuthorizationClaims| claims.audience = "wrong".into(),
        |claims: &mut ManagedInferenceAuthorizationClaims| claims.expires_at_ms = 1_699_999_000_000,
        |claims: &mut ManagedInferenceAuthorizationClaims| claims.grant_epoch = 8,
    ] {
        let mut tampered = authorization.clone();
        mutate(&mut tampered.claims);
        let context = verification_context(&keys);
        assert!(verify_authorization(&tampered, &context).is_err());
    }

    let mut tampered = authorization;
    tampered.signature = URL_SAFE_NO_PAD.encode([0_u8; 64]);
    let context = verification_context(&keys);
    assert!(verify_authorization(&tampered, &context).is_err());
}

#[test]
fn signed_authorization_enforces_the_published_lifetime_boundary() {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
    let mut claims = fixture_claims();
    claims.expires_at_ms = claims.issued_at_ms + MAX_AUTHORIZATION_LIFETIME_MS;
    claims.scope_digest = scope_digest(&fixture_scope()).unwrap();
    let authorization = sign_authorization(&claims, document.as_ref()).unwrap();
    let keys = BTreeMap::from([(claims.key_id.clone(), pair.public_key().as_ref().to_vec())]);
    let mut context = verification_context(&keys);
    context.clock_skew_ms = 0;
    context.now_ms = claims.issued_at_ms + 900_000;
    assert!(verify_authorization(&authorization, &context).is_ok());
    context.now_ms += 1;
    assert!(matches!(
        verify_authorization(&authorization, &context),
        Err(AuthorizationVerificationError::Expired)
    ));
    claims.expires_at_ms = claims.issued_at_ms + 900_001;
    assert!(matches!(
        sign_authorization(&claims, document.as_ref()),
        Err(AuthorizationVerificationError::InvalidLifetime)
    ));
}
