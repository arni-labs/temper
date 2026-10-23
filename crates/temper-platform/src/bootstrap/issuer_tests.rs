//! A start applies the environment's issuer keys only while the stored issuer
//! still takes its keys from the environment.
use super::*;

const TENANT: &str = "issuer-tenant";
const ISSUER: &str = "https://issuer.example.test";
const AUDIENCE: &str = "https://audience.example.test";

fn jwks(kid: &str) -> String {
    format!(r#"{{"keys":[{{"kty":"EC","crv":"P-256","kid":"{kid}","x":"x","y":"y"}}]}}"#)
}

async fn state_with_identity_specs() -> PlatformState {
    let state = PlatformState::new(None);
    bootstrap_identity_specs(&state, TENANT)
        .await
        .expect("identity specs should register");
    state
}

/// One start with the environment holding key set `kid`.
async fn start(state: &PlatformState, kid: &str) {
    let outcome = apply_environment_issuer(state, TENANT, ISSUER, &jwks(kid), AUDIENCE, "ES256")
        .await
        .expect("environment issuer applies");
    assert_eq!(outcome, IssuerBootstrapOutcome::Registered);
}

async fn act(state: &PlatformState, action: &str, params: serde_json::Value) {
    let response = state
        .server
        .dispatch_tenant_action(
            &temper_runtime::tenant::TenantId::new(TENANT),
            "TrustedIssuer",
            ISSUER,
            action,
            params,
            &temper_server::request_context::AgentContext::system(),
        )
        .await
        .expect("dispatch reaches the issuer");
    assert!(response.success, "{action} failed: {:?}", response.error);
}

async fn stored(state: &PlatformState) -> temper_server::entity_actor::EntityState {
    stored_trusted_issuer(
        state,
        &temper_runtime::tenant::TenantId::new(TENANT),
        ISSUER,
    )
    .await
    .expect("the issuer is stored")
}

async fn stored_jwks(state: &PlatformState) -> String {
    stored(state).await.fields["jwks_json"]
        .as_str()
        .expect("jwks_json is a string")
        .to_string()
}

#[tokio::test]
async fn a_changed_environment_key_applies_to_an_issuer_the_environment_holds() {
    let state = state_with_identity_specs().await;
    start(&state, "kid-a").await;
    assert_eq!(stored_jwks(&state).await, jwks("kid-a"));

    start(&state, "kid-b").await;
    assert_eq!(stored_jwks(&state).await, jwks("kid-b"));
    assert_ne!(
        stored(&state).await.booleans.get("keys_rotated"),
        Some(&true)
    );
}

#[tokio::test]
async fn a_rotated_issuer_key_is_kept_at_the_next_start() {
    let state = state_with_identity_specs().await;
    start(&state, "kid-a").await;
    act(
        &state,
        "RotateIssuerKeys",
        serde_json::json!({ "jwks_json": jwks("kid-b") }),
    )
    .await;
    assert_eq!(
        stored(&state).await.booleans.get("keys_rotated"),
        Some(&true)
    );

    start(&state, "kid-a").await;
    assert_eq!(stored_jwks(&state).await, jwks("kid-b"));
    start(&state, "kid-c").await;
    assert_eq!(stored_jwks(&state).await, jwks("kid-b"));
}

#[tokio::test]
async fn a_start_leaves_a_suspended_or_revoked_issuer_as_it_is() {
    for (action, status) in [("SuspendIssuer", "Suspended"), ("RevokeIssuer", "Revoked")] {
        let state = state_with_identity_specs().await;
        start(&state, "kid-a").await;
        act(&state, action, serde_json::json!({})).await;

        start(&state, "kid-b").await;
        let issuer = stored(&state).await;
        assert_eq!(issuer.status, status);
        assert_eq!(
            issuer.fields["jwks_json"].as_str(),
            Some(jwks("kid-a").as_str())
        );
    }
}
