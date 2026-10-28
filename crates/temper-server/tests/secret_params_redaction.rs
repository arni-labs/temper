//! A request's secret-named arguments never come back to anyone who reads what
//! the server recorded about it.
//!
//! One action call over HTTP carries passwords, tokens, card details and keys,
//! at the top level, nested in objects and inside arrays, next to ordinary
//! fields whose names only look secret. The call fails its guard, so the
//! server keeps its arguments as evidence of an unmet intent, which an
//! operator reads over HTTP at `/observe/evolution/intent-evidence`. The test
//! reads that evidence, the call's own response, and every log line and span
//! field the server wrote, and checks that no secret value is in any of them
//! while every ordinary value is kept as sent.
#![cfg(feature = "observe")]

mod common;
mod telemetry_capture;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const INTENT: &str = "submit the order with the stored card";
const PLACEHOLDER: &str = "[redacted]";

/// The arguments of the call: secrets, and ordinary fields that only look
/// secret. Every secret value is unique text, so its absence can be checked
/// anywhere.
fn arguments() -> Value {
    json!({
        "ShippingAddressId": "addr-1",
        "PaymentMethod": "card",
        // Secret by name, in any spelling.
        "password": "SECRET-password",
        "api_token": "SECRET-api-token",
        "apiKey": "SECRET-api-key",
        "Authorization": "SECRET-authorization",
        "card_number": "SECRET-card-number",
        "cvv": 987_654_321,
        "client_secret": "SECRET-client-secret",
        // Secret only as the whole name.
        "auth": "SECRET-auth",
        "pin": "SECRET-pin",
        // Nested, and inside arrays.
        "connection": { "host": "db-host", "password": "SECRET-nested-password" },
        "accounts": [
            { "id": "account-a", "access_token": "SECRET-arrayed-token" },
            { "id": "account-b" },
        ],
        // A secret-named object, whose own fields are not secret-named.
        "credentials": { "user": "SECRET-user-inside-credentials", "hint": "SECRET-hint-inside-credentials" },
        // Ordinary fields whose names only look secret.
        "oauth": "provider",
        "spin": 3,
        "checkin": "10:00",
        "prompt_tokens": 128,
        "completion_token_ids": [1, 2, 3],
        "idempotency_key": "idem-1",
        "author": "rita",
        "keyboard": "qwerty",
        "monkey": "george",
        "public_key_id": "kid-1",
    })
}

/// Secret values as text, as they would appear in any rendering.
const SECRET_TEXTS: &[&str] = &[
    "SECRET-password",
    "SECRET-api-token",
    "SECRET-api-key",
    "SECRET-authorization",
    "SECRET-card-number",
    "987654321",
    "SECRET-client-secret",
    "SECRET-auth",
    "SECRET-pin",
    "SECRET-nested-password",
    "SECRET-arrayed-token",
    "SECRET-user-inside-credentials",
    "SECRET-hint-inside-credentials",
];

/// JSON pointers into the recorded arguments that must read as the
/// placeholder.
const REDACTED_AT: &[&str] = &[
    "/password",
    "/api_token",
    "/apiKey",
    "/Authorization",
    "/card_number",
    "/cvv",
    "/client_secret",
    "/auth",
    "/pin",
    "/connection/password",
    "/accounts/0/access_token",
    "/credentials",
];

/// JSON pointers that must read exactly as sent.
const KEPT_AT: &[&str] = &[
    "/ShippingAddressId",
    "/PaymentMethod",
    "/connection/host",
    "/accounts/0/id",
    "/accounts/1/id",
    "/oauth",
    "/spin",
    "/checkin",
    "/prompt_tokens",
    "/completion_token_ids",
    "/idempotency_key",
    "/author",
    "/keyboard",
    "/monkey",
    "/public_key_id",
];

/// An Order tenant on a durable store, so the server keeps what it records.
async fn build_state(db_dir: &std::path::Path) -> ServerState {
    let db_url = format!("file:{}", db_dir.join("secret-params.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(common::CSDL_XML).expect("CSDL parses"),
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("secret-params-redaction"), registry);
    {
        let mut registry = state.registry.write().expect("registry lock");
        registry.set_verification_status(
            &TenantId::default(),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-10-28T00:00:00Z".to_string(),
            }),
        );
    }
    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            "permit(principal, action, resource);",
        )
        .expect("the test policy parses");
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, String) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("secret-params-agent", "test-agent", None),
        ));
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("the body reads");
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn post(path: &str, body: &Value) -> Request<Body> {
    Request::post(path)
        .header("Content-Type", "application/json")
        .header("X-Session-Id", "secret-params-session")
        .header("X-Intent", INTENT)
        .body(Body::from(body.to_string()))
        .expect("the request builds")
}

/// The recorded arguments of the failed call, as the intent-evidence route
/// returns them, with the whole response text. Recording runs in the
/// background, so wait for it.
async fn recorded_arguments(state: &ServerState) -> (Value, String) {
    let mut last = String::new();
    for _ in 0..200 {
        let (status, body) = send(
            state,
            Request::get("/observe/evolution/intent-evidence")
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "intent evidence reads: {body}");
        let evidence: Value = serde_json::from_str(&body).expect("evidence is JSON");
        let sample = evidence["intent_candidates"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|candidate| {
                candidate["attempted_actions"]
                    .as_array()
                    .is_some_and(|actions| actions.iter().any(|a| a == "SubmitOrder"))
                    && candidate["sample_body"].is_object()
            })
            .map(|candidate| candidate["sample_body"].clone());
        if let Some(sample) = sample {
            return (sample, body);
        }
        last = body;
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the failed call's arguments were never recorded as evidence: {last}");
}

#[tokio::test]
async fn secret_named_arguments_never_come_back_and_ordinary_ones_do() {
    let telemetry = telemetry_capture::telemetry();
    let db_dir = tempfile::tempdir().expect("a temporary directory");
    let state = build_state(db_dir.path()).await;

    let (status, body) = send(
        &state,
        post("/tdata/Orders", &json!({ "id": "ord-secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "the order is created: {body}");

    // SubmitOrder needs items; the new order has none, so the guard refuses it
    // and the server keeps the call as evidence of an unmet intent.
    let sent = arguments();
    let (status, response) = send(
        &state,
        post("/tdata/Orders('ord-secret')/Temper.SubmitOrder", &sent),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "the guard refuses the call: {response}"
    );

    let (recorded, evidence) = recorded_arguments(&state).await;

    let mut wrong = Vec::new();
    for pointer in REDACTED_AT {
        let value = recorded.pointer(pointer);
        if value != Some(&json!(PLACEHOLDER)) {
            wrong.push(format!(
                "{pointer} should read {PLACEHOLDER:?}, reads {value:?}"
            ));
        }
    }
    for pointer in KEPT_AT {
        let (value, expected) = (recorded.pointer(pointer), sent.pointer(pointer));
        if value != expected {
            wrong.push(format!(
                "{pointer} should read {expected:?}, reads {value:?}"
            ));
        }
    }
    let logs = telemetry.everything();
    for (surface, text) in [
        ("the call's response", &response),
        ("the intent evidence", &evidence),
        ("the server's logs and spans", &logs),
    ] {
        for secret in SECRET_TEXTS {
            if text.contains(secret) {
                wrong.push(format!("{surface} contains {secret}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "what the server recorded about the call is wrong: {wrong:#?}\nrecorded arguments: {recorded:#}"
    );
}
