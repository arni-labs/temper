//! `POST /api/runtime/wake` is authorized like any Cedar-governed call: the
//! caller is authenticated, then Cedar decides `wake` on `Runtime::"runtime"`.
//!
//! The control plane presents the app owner's token, a trusted-issuer JWT with
//! only a `sub` (a Customer), and the app's Cedar permits that owner. Another
//! Customer of the same issuer is refused, and a caller with no token is not
//! authenticated. Neither of them learns when the process next has work.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use temper_platform::{PlatformState, bootstrap_agent_specs, bootstrap_system_tenant};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use tower::ServiceExt;

const TENANT: &str = "default";
const ISSUER: &str = "https://issuer.wake.local";
const AUD: &str = "https://issuer.wake.local/tempers/t1/app1";
const OWNER: &str = "owner-sub";

/// The permit the app's Cedar gives its owner, as the control plane's
/// app build writes it beside the owner's vault permit.
const OWNER_WAKES: &str = r#"
permit(principal is Customer, action == Action::"wake", resource is Runtime)
when { principal.id == "owner-sub" };
"#;

fn b64(v: &serde_json::Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

/// A person's token: `sub` only, as the control plane's owner token is.
fn person_token(key: &SigningKey, sub: &str) -> String {
    let header = serde_json::json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" });
    let claims = serde_json::json!({
        "iss": ISSUER, "aud": AUD, "sub": sub, "role": "contributor",
        "auth_generation": 0, "nbf": 0, "exp": 4_102_444_800i64,
    });
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let signature: Signature = key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

fn jwks_json(key: &SigningKey) -> String {
    let point = key.verifying_key().to_encoded_point(false);
    serde_json::json!({ "keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1",
        "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
        "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
    }]})
    .to_string()
}

/// A platform whose tenant trusts the issuer and permits the owner to wake.
async fn router(key: &SigningKey) -> axum::Router {
    let state = PlatformState::new(None);
    let cache = BTreeMap::new();
    bootstrap_system_tenant(&state, &cache);
    bootstrap_agent_specs(&state, TENANT, false, &cache);
    state
        .server
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "TrustedIssuer",
            ISSUER,
            "RegisterIssuer",
            serde_json::json!({
                "issuer": ISSUER,
                "jwks_json": jwks_json(key),
                "audience": AUD,
                "algorithms": "ES256",
                "description": "wake test issuer",
                "created_by": "wake test",
            }),
            &AgentContext::system(),
        )
        .await
        .expect("register issuer");
    state
        .server
        .authz
        .reload_tenant_policies(TENANT, OWNER_WAKES)
        .expect("the owner's permit parses");
    temper_platform::router::build_platform_router(state)
}

/// POST the wake, with `token` as the bearer when given. Returns the status
/// and the body's text.
async fn wake(app: &axum::Router, token: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::post("/api/runtime/wake").header("X-Tenant-Id", TENANT);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn only_the_permitted_owner_wakes_the_process() {
    let key = SigningKey::from_slice(&[11u8; 32]).unwrap();
    let app = router(&key).await;

    let (status, body) = wake(&app, Some(&person_token(&key, OWNER))).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the owner wakes the process: {body}"
    );
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(answer["fired"], 0, "nothing was due: {body}");
    assert!(
        answer.get("next_due_at").is_some(),
        "the owner hears when the process next has work: {body}"
    );

    let (status, body) = wake(&app, Some(&person_token(&key, "someone-else"))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "another person is refused: {body}"
    );
    assert!(
        !body.contains("next_due_at"),
        "a refused caller learns nothing: {body}"
    );

    let (status, body) = wake(&app, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no token, no wake: {body}"
    );
    assert!(
        !body.contains("next_due_at"),
        "an unauthenticated caller learns nothing: {body}"
    );
}
