//! `POST /api/runtime/wake` is authorized like any Cedar-governed call: the
//! caller is authenticated, then Cedar decides `wake` on `Runtime::"runtime"`.
//!
//! The control plane presents the app owner's token, a trusted-issuer JWT with
//! only a `sub` (a Customer), and the app's Cedar permits that owner. Another
//! Customer of the same issuer is refused, and a caller with no token is not
//! authenticated. Neither of them learns when the process next has work.
//!
//! Only a wake by the agent principal the process is told is its control
//! plane stands for a report delivered. The owner waking by hand gets the
//! same answer, and the process still reports to the control plane.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use temper_platform::{PlatformState, bootstrap_agent_specs, bootstrap_system_tenant};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use temper_server::wake::{SendError, WakeReport, WakeSink};
use tower::ServiceExt;

const TENANT: &str = "default";
const ISSUER: &str = "https://issuer.wake.local";
const AUD: &str = "https://issuer.wake.local/tempers/t1/app1";
const OWNER: &str = "owner-sub";
/// The agent principal the control plane wakes the process as.
const CONTROL_PLANE: &str = "waker:app1";

/// The permit the app's Cedar gives its owner, as the control plane's
/// app build writes it beside the owner's vault permit.
const OWNER_WAKES: &str = r#"
permit(principal is Customer, action == Action::"wake", resource is Runtime)
when { principal.id == "owner-sub" };
permit(principal is Agent, action == Action::"wake", resource is Runtime)
when { principal.id == "waker:app1" };
"#;

fn b64(v: &serde_json::Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

/// A person's token: `sub` only.
fn person_token(key: &SigningKey, sub: &str) -> String {
    signed(
        key,
        serde_json::json!({ "sub": sub, "role": "contributor" }),
    )
}

/// The control plane's token: an agent acting for the owner.
fn control_plane_token(key: &SigningKey) -> String {
    signed(
        key,
        serde_json::json!({ "sub": OWNER, "agent_type": "control-plane", "client_id": CONTROL_PLANE }),
    )
}

fn signed(key: &SigningKey, mut claims: serde_json::Value) -> String {
    let header = serde_json::json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" });
    let common = serde_json::json!({
        "iss": ISSUER, "aud": AUD, "auth_generation": 0, "nbf": 0, "exp": 4_102_444_800i64,
    });
    for (name, value) in common.as_object().unwrap() {
        claims[name] = value.clone();
    }
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

/// A control plane that takes no report, keeping each one sent.
#[derive(Default)]
struct Down {
    sent: Mutex<Vec<WakeReport>>,
}

#[async_trait::async_trait]
impl WakeSink for Down {
    async fn send(&self, report: &WakeReport, _traceparent: Option<&str>) -> Result<(), SendError> {
        self.sent.lock().unwrap().push(report.clone());
        Err(SendError::Undelivered("down".to_string()))
    }
}

/// A platform whose tenant trusts the issuer and permits the owner and the
/// control plane to wake, reporting to `sink`.
async fn router(key: &SigningKey) -> axum::Router {
    router_reporting(key, Arc::new(Down::default())).await
}

async fn router_reporting(key: &SigningKey, sink: Arc<Down>) -> axum::Router {
    let state = PlatformState::new(None);
    state.server.set_wake_sink(sink);
    state
        .server
        .set_wake_control_plane(CONTROL_PLANE.to_string());
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
    assert!(
        answer["reported_at"].is_string() && answer["seq"].is_u64(),
        "the answer is a report, ordered with the others: {body}"
    );

    let (status, body) = wake(&app, Some(&person_token(&key, "someone-else"))).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "another person is refused: {body}"
    );
    assert!(
        ["next_due_at", "reported_at", "seq"]
            .iter()
            .all(|field| !body.contains(field)),
        "a refused caller learns nothing: {body}"
    );

    let (status, body) = wake(&app, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "no token, no wake: {body}"
    );
    assert!(
        ["next_due_at", "reported_at", "seq"]
            .iter()
            .all(|field| !body.contains(field)),
        "an unauthenticated caller learns nothing: {body}"
    );
}

/// The reports sent after one wake by `token`, on a fresh process.
async fn reports_after_wake(key: &SigningKey, token: &str) -> Vec<WakeReport> {
    let sink = Arc::new(Down::default());
    let app = router_reporting(key, sink.clone()).await;
    assert!(
        sink.sent.lock().unwrap().is_empty(),
        "nothing is reported before the wake"
    );
    let (status, body) = wake(&app, Some(token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let answer: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        answer["reported_at"].is_string() && answer["seq"].is_u64(),
        "every permitted caller gets the same answer: {body}"
    );
    // Let the report's send run.
    tokio::time::sleep(Duration::from_millis(50)).await;
    sink.sent.lock().unwrap().clone()
}

#[tokio::test]
async fn only_the_control_planes_wake_stands_for_a_report() {
    let key = SigningKey::from_slice(&[11u8; 32]).unwrap();
    let sent = reports_after_wake(&key, &control_plane_token(&key)).await;
    assert!(
        sent.is_empty(),
        "the control plane holds its wake's answer, so nothing is left to report: {sent:?}"
    );
    let sent = reports_after_wake(&key, &person_token(&key, OWNER)).await;
    assert!(
        !sent.is_empty(),
        "the owner's wake never reaches the control plane, so the process still reports"
    );
}
