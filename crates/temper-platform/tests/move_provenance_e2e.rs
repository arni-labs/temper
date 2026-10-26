//! Each move records who made it, as the kernel verified them, and the
//! kernel's own dispatch path that carried it.
//!
//! Exercised through the real platform router: the bearer middleware resolves
//! a real ES256 token against a registered `TrustedIssuer` (or an API key
//! against the `AgentCredential` registry), the move is written to the durable
//! journal, and the record's `$history` read returns `made_by` and `source`
//! with each event. The Cedar check span names the same principal.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
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

/// Create a Task and claim it, both as `bearer`. A Task, not one of the
/// identity types the platform keeps to itself, so any principal may move it.
async fn create_and_claim(app: &axum::Router, id: &str, bearer: &str) {
    let body = json!({ "Id": id, "title": "provenance" });
    let (status, body) = send(app, post("/tdata/Tasks", bearer, body)).await;
    assert_eq!(status, StatusCode::CREATED, "create {id}: {body:?}");
    let (status, body) = send(app, claim(id, bearer)).await;
    assert_eq!(status, StatusCode::OK, "claim {id}: {body:?}");
}

fn claim(id: &str, bearer: &str) -> Request<Body> {
    post(
        &format!("/tdata/Tasks('{id}')/Temper.Claim"),
        bearer,
        json!({}),
    )
}

/// The record's history, read by the operator.
async fn history(app: &axum::Router, id: &str) -> Vec<Value> {
    let request = Request::get(format!("/tdata/Tasks('{id}')/$history"))
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
    create_and_claim(&app, "made-by-human", &token).await;

    let events = history(&app, "made-by-human").await;
    for action in ["Created", "Claim"] {
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
    create_and_claim(&app, "made-by-agent", &token).await;

    let events = history(&app, "made-by-agent").await;
    let claim = event(&events, "Claim");
    assert_eq!(claim["source"], "http", "{claim:?}");
    let made_by = &claim["made_by"];
    assert_eq!(made_by["id"], "kc_agent_provenance", "{claim:?}");
    assert_eq!(made_by["kind"], "Agent", "{claim:?}");
    assert_eq!(made_by["agent_type"], "contributor", "{claim:?}");
    assert_eq!(made_by["agent_type_verified"], true, "{claim:?}");
    assert_eq!(made_by["acting_for"], "human-rita", "{claim:?}");
    assert_eq!(made_by["role"], "owner", "{claim:?}");
    assert_eq!(made_by["issuer"], ISSUER, "{claim:?}");
    assert_eq!(made_by["token"], "access", "{claim:?}");
    assert_eq!(made_by["client_id"], "kc_agent_provenance", "{claim:?}");
    assert_eq!(made_by["grant_id"], "grant-provenance", "{claim:?}");
}

#[tokio::test]
async fn an_api_key_records_its_agent_and_no_token() {
    let sk = SigningKey::from_slice(&[13u8; 32]).unwrap();
    let app = platform(&sk).await;
    create_and_claim(&app, "made-by-key", OPERATOR_KEY).await;

    let events = history(&app, "made-by-key").await;
    let claim = event(&events, "Claim");
    assert_eq!(claim["source"], "http", "{claim:?}");
    let made_by = &claim["made_by"];
    assert_eq!(made_by["id"], "operator", "{claim:?}");
    assert_eq!(made_by["kind"], "Agent", "{claim:?}");
    assert_eq!(made_by["agent_type"], "operator", "{claim:?}");
    assert_eq!(made_by["agent_type_verified"], true, "{claim:?}");
    for field in ["issuer", "token", "client_id", "grant_id"] {
        assert_eq!(made_by[field], Value::Null, "{field}: {claim:?}");
    }
}

#[tokio::test]
async fn a_forged_or_unknown_issuer_token_is_refused_and_records_nothing() {
    let sk = SigningKey::from_slice(&[14u8; 32]).unwrap();
    let app = platform(&sk).await;
    let (status, body) = send(
        &app,
        post(
            "/tdata/Tasks",
            OPERATOR_KEY,
            json!({ "Id": "untouched", "title": "provenance" }),
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
        let (status, _) = send(&app, claim("untouched", &token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        history(&app, "untouched").await,
        before,
        "a refused token must not record a move"
    );
}

/// Every span the kernel emits through the global tracer, kept in memory. The
/// global tracer is one per process, so it is installed once for this test
/// binary and each test reads only the spans of its own record.
fn exported_spans() -> &'static InMemorySpanExporter {
    static EXPORTER: OnceLock<InMemorySpanExporter> = OnceLock::new();
    EXPORTER.get_or_init(|| {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider);
        exporter
    })
}

/// The attributes of the Cedar check span for `action` on record `id`.
fn authz_span(action: &str, id: &str) -> BTreeMap<String, String> {
    let name = format!("authz.{action}");
    exported_spans()
        .get_finished_spans()
        .expect("finished spans")
        .into_iter()
        .filter(|span| span.name == name)
        .map(|span| {
            span.attributes
                .iter()
                .map(|kv| (kv.key.to_string(), kv.value.to_string()))
                .collect::<BTreeMap<_, _>>()
        })
        .find(|attrs| attrs.get("entity_id").map(String::as_str) == Some(id))
        .unwrap_or_else(|| panic!("no {name} span for {id}"))
}

#[tokio::test]
async fn the_cedar_check_span_names_the_principal_as_verified() {
    exported_spans();
    let sk = SigningKey::from_slice(&[16u8; 32]).unwrap();
    let app = platform(&sk).await;
    let token = mint(&sk, access_claims());
    create_and_claim(&app, "span-by-agent", &token).await;
    create_and_claim(&app, "span-by-key", OPERATOR_KEY).await;

    let agent = authz_span("Claim", "span-by-agent");
    let expected = [
        ("principal_kind", "Agent"),
        ("principal_id", "kc_agent_provenance"),
        ("principal_issuer", ISSUER),
        ("principal_token_kind", "access"),
        ("principal_client_id", "kc_agent_provenance"),
        ("principal_grant_id", "grant-provenance"),
        ("principal_acting_for", "human-rita"),
        ("principal_agent_type", "contributor"),
        ("principal_role", "owner"),
    ];
    for (key, value) in expected {
        assert_eq!(
            agent.get(key).map(String::as_str),
            Some(value),
            "{key}: {agent:?}"
        );
    }
    assert!(
        agent.values().all(|value| !value.contains(&token)),
        "the bearer token must never reach a span: {agent:?}"
    );

    let key = authz_span("Claim", "span-by-key");
    assert_eq!(
        key.get("principal_id").map(String::as_str),
        Some("operator"),
        "{key:?}"
    );
    assert_eq!(
        key.get("principal_agent_type").map(String::as_str),
        Some("operator"),
        "{key:?}"
    );
    for absent in [
        "principal_issuer",
        "principal_token_kind",
        "principal_client_id",
        "principal_grant_id",
    ] {
        assert!(!key.contains_key(absent), "{absent}: {key:?}");
    }
    assert!(
        key.values().all(|value| !value.contains(OPERATOR_KEY)),
        "the API key must never reach a span: {key:?}"
    );
}
