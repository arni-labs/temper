//! A stored-field reference is made by the kernel, never written by a caller.
//!
//! A value too large to keep in its record is stored as a blob, and the field
//! holds a reference to it (`__temper_blob_ref`). Everything that reads the
//! field reads the blob the reference names: a read, a guard, a module's
//! `host_read_field`. A caller that wrote a reference into a field could name
//! any blob of the tenant, a field of a record it may not read, and read it
//! back through the record it writes. Every write now refuses a reference its
//! record does not already hold: a move from any caller, a caller's create, a
//! field update. Sending back the reference the record holds still goes through.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::CSDL_XML;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const SERVICE: &str = "field-reference-writes-test";

const NOTED: &str = r#"
[automaton]
name = "Order"
states = ["Open"]
initial = "Open"

[[action]]
name = "Note"
kind = "input"
from = ["Open"]
params = ["note"]
"#;

/// A reference to a blob the caller did not write: the key names any blob.
fn forged() -> serde_json::Value {
    json!({
        "__temper_blob_ref": format!("field-overflow/sha256/{}.json", "a".repeat(64)),
        "__temper_blob_size": 12,
        "__temper_blob_encoding": "json",
    })
}

fn state_with(name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    registry
        .try_register_tenant("default", csdl, CSDL_XML.to_string(), &[("Order", NOTED)])
        .expect("the tenant registers");
    let state = ServerState::from_registry(ActorSystem::new(name), registry);
    state
        .authz
        .reload_tenant_policies(
            "default",
            &format!(
                r#"permit(principal == Agent::"service:{SERVICE}", action, resource is Order);"#
            ),
        )
        .expect("the policy loads");
    state
}

async fn note(state: &ServerState, id: &str) -> serde_json::Value {
    state
        .get_tenant_entity_state(&TenantId::default(), "Order", id)
        .await
        .expect("the record reads")
        .state
        .fields
        .get("note")
        .cloned()
        .unwrap_or_default()
}

async fn send_note(state: &ServerState, id: &str, value: serde_json::Value) -> (bool, String) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            id,
            "Note",
            json!({ "note": value }),
            &AgentContext::for_service(SERVICE),
        )
        .await
        .expect("the move answers");
    (response.success, response.error.unwrap_or_default())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_cannot_write_a_reference() {
    let state = state_with("field-reference-move");
    let (stored, error) = send_note(&state, "o1", forged()).await;
    assert!(!stored, "the move stored a reference the caller wrote");
    assert!(error.contains("stored-field reference"), "{error}");
    assert_ne!(note(&state, "o1").await, forged());

    // Nested in a value too.
    let (stored, _) = send_note(&state, "o1", json!({ "parts": [forged()] })).await;
    assert!(
        !stored,
        "the move stored a nested reference the caller wrote"
    );

    let (stored, error) = send_note(&state, "o1", json!("a plain note")).await;
    assert!(stored, "{error}");
    assert_eq!(note(&state, "o1").await, json!("a plain note"));
}

/// A caller's create comes in as an OData collection create; the kernel's
/// own creates (a raw ingest) store the references they have just made.
#[tokio::test(flavor = "multi_thread")]
async fn a_create_cannot_write_a_reference() {
    let state = state_with("field-reference-create");
    let mut request = Request::builder()
        .method("POST")
        .uri("/tdata/Orders")
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({"id": "o2", "Notes": forged()}).to_string(),
        ))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity("service", SERVICE, None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.contains("stored-field reference"), "{body}");
    assert!(!state.entity_exists(&TenantId::default(), "Order", "o2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_update_cannot_write_a_reference() {
    let state = state_with("field-reference-update");
    let (stored, error) = send_note(&state, "o3", json!("first")).await;
    assert!(stored, "{error}");
    let refused = state
        .update_tenant_entity_fields(
            &TenantId::default(),
            "Order",
            "o3",
            json!({"note": forged()}),
            false,
        )
        .await
        .expect_err("the update stored a reference the caller wrote");
    assert!(refused.contains("stored-field reference"), "{refused}");
    assert_eq!(note(&state, "o3").await, json!("first"));
}
