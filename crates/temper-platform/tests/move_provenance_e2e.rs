//! Each move records who made it, as the kernel verified them, and the
//! kernel's own dispatch path that carried it.
//!
//! Exercised through the real platform router: the bearer middleware resolves
//! a real ES256 token against a registered `TrustedIssuer` (or an API key
//! against the `AgentCredential` registry), the move is written to the durable
//! journal, and the record's `$history` read returns `made_by` and `source`
//! with each event.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use temper_platform::{PlatformState, bootstrap_agent_specs, bootstrap_system_tenant};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use tower::ServiceExt;

mod common;

use common::http::body_json;

const TENANT: &str = "default";
const ISSUER: &str = "https://issuer.provenance.local";
const AUD: &str = "temper-provenance";
const OPERATOR_KEY: &str = "provenance-operator-key";
/// Year 2100: valid whatever the wall clock says.
const FAR_FUTURE: i64 = 4_102_444_800;

fn b64(v: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

fn mint(sk: &SigningKey, claims: Value) -> String {
    let header = json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" });
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let sig: Signature = sk.sign(signing_input.as_bytes());
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}

fn jwks_json(sk: &SigningKey) -> String {
    let point = sk.verifying_key().to_encoded_point(false);
    json!({
        "keys": [{
            "kty": "EC", "crv": "P-256", "kid": "k1",
            "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
            "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
        }]
    })
    .to_string()
}

/// A token for the human themselves: only a `sub`, no `agent_type`.
fn human_claims() -> Value {
    json!({
        "iss": ISSUER, "sub": "human-rita", "aud": AUD, "role": "owner",
        "nbf": 0, "exp": FAR_FUTURE,
    })
}

/// An access token: an agent (`client_id`, `agent_type`) acting for the
/// human `sub` under a grant.
fn access_claims() -> Value {
    json!({
        "iss": ISSUER, "sub": "human-rita", "aud": AUD, "role": "owner",
        "client_id": "kc_agent_provenance", "agent_type": "contributor",
        "grant_id": "grant-provenance", "nbf": 0, "exp": FAR_FUTURE,
    })
}

/// A platform on a durable journal with a registered issuer, a tenant
/// operator API key and a policy that lets everyone do everything: this test
/// is about what a move records, not about who may make it.
async fn platform(sk: &SigningKey) -> axum::Router {
    let store = temper_store_sim::SimEventStore::no_faults(41);
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_sim(store, None));
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
            json!({
                "issuer": ISSUER, "jwks_json": jwks_json(sk), "audience": AUD,
                "algorithms": "ES256", "description": "provenance issuer",
                "created_by": "test",
            }),
            &AgentContext::system(),
        )
        .await
        .expect("register issuer");
    temper_platform::bootstrap_operator_credential(&state, OPERATOR_KEY, TENANT)
        .await
        .expect("operator credential");
    state
        .server
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("policy should parse");
    temper_platform::router::build_platform_router(state)
}

async fn send(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = if status == StatusCode::UNAUTHORIZED {
        Value::Null
    } else {
        body_json(response).await
    };
    (status, body)
}

fn post(path: &str, bearer: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {bearer}"))
        .header("X-Tenant-Id", TENANT)
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Create an AgentType and move it to Active, both as `bearer`.
async fn create_and_define(app: &axum::Router, id: &str, bearer: &str) {
    let (status, body) = send(app, post("/tdata/AgentTypes", bearer, json!({ "id": id }))).await;
    assert_eq!(status, StatusCode::CREATED, "create {id}: {body:?}");
    let (status, body) = send(app, define(id, bearer)).await;
    assert_eq!(status, StatusCode::OK, "define {id}: {body:?}");
}

fn define(id: &str, bearer: &str) -> Request<Body> {
    post(
        &format!("/tdata/AgentTypes('{id}')/Temper.Agent.Define"),
        bearer,
        json!({
            "name": format!("type-{id}"), "system_prompt": "test", "tool_set": "local",
            "model": "claude-sonnet-4-6", "max_turns": "200", "adapter_config": "{}",
            "default_budget_cents": "0",
        }),
    )
}

/// The record's history, read by the operator.
async fn history(app: &axum::Router, id: &str) -> Vec<Value> {
    let request = Request::get(format!("/tdata/AgentTypes('{id}')/$history"))
        .header("Authorization", format!("Bearer {OPERATOR_KEY}"))
        .header("X-Tenant-Id", TENANT)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(app, request).await;
    assert_eq!(status, StatusCode::OK, "history of {id}: {body:?}");
    body["events"].as_array().expect("events").clone()
}

fn event<'a>(events: &'a [Value], action: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["action"] == action)
        .unwrap_or_else(|| panic!("no {action} event in {events:?}"))
}

#[tokio::test]
async fn a_human_token_records_the_human_and_its_issuer() {
    let sk = SigningKey::from_slice(&[11u8; 32]).unwrap();
    let app = platform(&sk).await;
    let token = mint(&sk, human_claims());
    create_and_define(&app, "made-by-human", &token).await;

    let events = history(&app, "made-by-human").await;
    for action in ["Created", "Define"] {
        let event = event(&events, action);
        assert_eq!(event["source"], "http", "{event:?}");
        let made_by = &event["made_by"];
        assert_eq!(made_by["id"], "human-rita", "{event:?}");
        assert_eq!(made_by["kind"], "Customer", "{event:?}");
        assert_eq!(made_by["agent_type"], Value::Null, "{event:?}");
        assert_eq!(made_by["acting_for"], Value::Null, "{event:?}");
        assert_eq!(made_by["role"], "owner", "{event:?}");
        assert_eq!(made_by["issuer"], ISSUER, "{event:?}");
        assert_eq!(made_by["token"], "human", "{event:?}");
        assert_eq!(made_by["client_id"], Value::Null, "{event:?}");
        assert_eq!(made_by["grant_id"], Value::Null, "{event:?}");
    }
}

#[tokio::test]
async fn an_access_token_records_the_agent_its_client_and_its_grant() {
    let sk = SigningKey::from_slice(&[12u8; 32]).unwrap();
    let app = platform(&sk).await;
    let token = mint(&sk, access_claims());
    create_and_define(&app, "made-by-agent", &token).await;

    let events = history(&app, "made-by-agent").await;
    let define = event(&events, "Define");
    assert_eq!(define["source"], "http", "{define:?}");
    let made_by = &define["made_by"];
    assert_eq!(made_by["id"], "kc_agent_provenance", "{define:?}");
    assert_eq!(made_by["kind"], "Agent", "{define:?}");
    assert_eq!(made_by["agent_type"], "contributor", "{define:?}");
    assert_eq!(made_by["agent_type_verified"], true, "{define:?}");
    assert_eq!(made_by["acting_for"], "human-rita", "{define:?}");
    assert_eq!(made_by["role"], "owner", "{define:?}");
    assert_eq!(made_by["issuer"], ISSUER, "{define:?}");
    assert_eq!(made_by["token"], "access", "{define:?}");
    assert_eq!(made_by["client_id"], "kc_agent_provenance", "{define:?}");
    assert_eq!(made_by["grant_id"], "grant-provenance", "{define:?}");
}

#[tokio::test]
async fn an_api_key_records_its_agent_and_no_token() {
    let sk = SigningKey::from_slice(&[13u8; 32]).unwrap();
    let app = platform(&sk).await;
    create_and_define(&app, "made-by-key", OPERATOR_KEY).await;

    let events = history(&app, "made-by-key").await;
    let define = event(&events, "Define");
    assert_eq!(define["source"], "http", "{define:?}");
    let made_by = &define["made_by"];
    assert_eq!(made_by["id"], "operator", "{define:?}");
    assert_eq!(made_by["kind"], "Agent", "{define:?}");
    assert_eq!(made_by["agent_type"], "operator", "{define:?}");
    assert_eq!(made_by["agent_type_verified"], true, "{define:?}");
    for field in ["issuer", "token", "client_id", "grant_id"] {
        assert_eq!(made_by[field], Value::Null, "{field}: {define:?}");
    }
}

#[tokio::test]
async fn a_forged_or_unknown_issuer_token_is_refused_and_records_nothing() {
    let sk = SigningKey::from_slice(&[14u8; 32]).unwrap();
    let app = platform(&sk).await;
    let (status, body) = send(
        &app,
        post(
            "/tdata/AgentTypes",
            OPERATOR_KEY,
            json!({ "id": "untouched" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let before = history(&app, "untouched").await;

    let rogue = SigningKey::from_slice(&[15u8; 32]).unwrap();
    let forged = mint(&rogue, access_claims());
    let mut unknown_claims = access_claims();
    unknown_claims["iss"] = json!("https://not-registered.example");
    let unknown = mint(&sk, unknown_claims);

    for token in [forged, unknown] {
        let (status, _) = send(&app, define("untouched", &token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        history(&app, "untouched").await,
        before,
        "a refused token must not record a move"
    );
}
