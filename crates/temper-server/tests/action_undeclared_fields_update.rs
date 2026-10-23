//! An outside call of a non-strict action whose body carries keys the action
//! does not declare writes those keys into fields, as a PATCH would. So it
//! needs the caller's own `update` grant on the entity as it is and as it would
//! be; declared params and inside dispatches are unchanged. Runs in the default
//! mode (`TEMPER_UNDECLARED_PARAMS` unset).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Notes" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Notes" EntityType="Temper.Notes.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[action]]
name = "Assign"
kind = "input"
from = ["Open"]
params = ["owner_id"]

[[action]]
name = "Comment"
kind = "input"
from = ["Open"]
params = ["text"]
"#;

/// Anyone may comment; only the note's owner may update it.
const POLICIES: &str = r#"
permit(principal, action == Action::"create", resource is Note);
permit(principal, action == Action::"read", resource is Note);
permit(principal, action == Action::"Assign", resource is Note);
permit(principal, action == Action::"Comment", resource is Note);
permit(principal, action == Action::"update", resource is Note)
  when { resource has owner_id && resource.owner_id == principal.id };
"#;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Note", NOTE_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("undeclared-update"), registry);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Note",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-23T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", POLICIES)
        .expect("policy should load");
    state
}

async fn post(
    state: &ServerState,
    principal: &str,
    path: &str,
    body: &str,
) -> (StatusCode, String) {
    let mut request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_verified_jwt(
                principal,
                PrincipalKind::Agent,
                Some("test"),
                None,
                None,
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn note(state: &ServerState) -> serde_json::Value {
    state
        .get_tenant_entity_state(&TenantId::default(), "Note", "n1")
        .await
        .expect("note should exist")
        .state
        .fields
}

async fn note_owned_by_alice() -> ServerState {
    let state = build_state();
    let (status, body) = post(&state, "alice", "/tdata/Notes", r#"{"Id":"n1"}"#).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = post(
        &state,
        "alice",
        "/tdata/Notes('n1')/Temper.Notes.Assign",
        r#"{"owner_id":"alice"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "a declared param needs no update grant: {body}"
    );
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn undeclared_keys_need_the_callers_update_grant() {
    let state = note_owned_by_alice().await;

    let (status, body) = post(
        &state,
        "bob",
        "/tdata/Notes('n1')/Temper.Notes.Comment",
        r#"{"text":"hi","owner_id":"bob"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bob may comment but may not update this note: {body}"
    );
    assert!(body.contains("decision"), "the refusal is recorded: {body}");
    let fields = note(&state).await;
    assert_eq!(fields["owner_id"], "alice", "nothing was written: {fields}");
    assert!(fields.get("text").is_none(), "{fields}");

    let (status, body) = post(
        &state,
        "bob",
        "/tdata/Notes('n1')/Temper.Notes.Comment",
        r#"{"text":"hi"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "declared params work as before: {body}"
    );
    assert_eq!(note(&state).await["text"], "hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_with_the_update_grant_still_writes_undeclared_keys() {
    let state = note_owned_by_alice().await;

    let (status, body) = post(
        &state,
        "alice",
        "/tdata/Notes('n1')/Temper.Notes.Comment",
        r#"{"text":"hi","label":"urgent"}"#,
    )
    .await;
    assert!(status.is_success(), "alice may update her note: {body}");
    let fields = note(&state).await;
    assert_eq!(fields["label"], "urgent", "{fields}");
    assert_eq!(fields["text"], "hi", "{fields}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_update_grant_must_hold_for_the_entity_as_it_would_be() {
    let state = note_owned_by_alice().await;

    let (status, body) = post(
        &state,
        "alice",
        "/tdata/Notes('n1')/Temper.Notes.Comment",
        r#"{"text":"hi","owner_id":"bob"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "after this write alice could no longer update the note: {body}"
    );
    assert_eq!(note(&state).await["owner_id"], "alice");
}

#[tokio::test(flavor = "multi_thread")]
async fn inside_dispatches_are_unchanged() {
    let state = note_owned_by_alice().await;

    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Note",
            "n1",
            "Comment",
            serde_json::json!({ "text": "inside", "label": "kept" }),
            &AgentContext::default(),
        )
        .await
        .expect("an inside dispatch runs");
    assert!(response.success, "{:?}", response.error);
    let fields = note(&state).await;
    assert_eq!(fields["label"], "kept", "{fields}");
}
