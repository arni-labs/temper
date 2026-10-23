//! A create whose key already names an entity answers only with what the
//! caller may read: the stored entity when the caller may read it, and
//! `409 Conflict` with no entity data otherwise. Nothing is written either way.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, VerificationStatus};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

/// Anyone may create; only the principal an order's number names may read
/// it. `OrderNumber` is a plain string with no relation, so a create needs no
/// other record to exist.
const ORDER_OWNER_READS: &str = r#"
permit(principal, action == Action::"create", resource is Order);
permit(principal, action == Action::"read", resource is Order)
when { resource has OrderNumber && resource.OrderNumber == principal.id };
"#;

/// Anyone may create; only the owner a note names may read it.
const NOTE_OWNER_READS: &str = r#"
permit(principal, action == Action::"create", resource is Note);
permit(principal, action == Action::"read", resource is Note)
when { resource has Owner && resource.Owner == principal.id };
"#;

const NOTE_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CreateExisting" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String" Nullable="false"/>
        <Property Name="Owner" Type="Edm.String" Nullable="false"/>
        <Property Name="Body" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Notes" EntityType="Temper.CreateExisting.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A spec with no actions: its creates take the data-only create path.
const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Kept"]
initial = "Kept"
"#;

fn mark_verified(state: &ServerState, entity_type: &str) {
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        entity_type,
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-23T00:00:00Z".into(),
        }),
    );
}

async fn send(
    state: &ServerState,
    method: &str,
    path: &str,
    principal_id: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let body = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext {
                principal: temper_authz::Principal {
                    id: principal_id.to_string(),
                    kind: temper_authz::PrincipalKind::Customer,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "create-existing-id-test".to_string(),
            },
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Two principals: alice owns a record, bob creates with the same key. Bob
/// gets 409 and none of alice's data; alice's record is unchanged; alice's
/// own repeated create still answers with her record.
async fn assert_create_with_existing_key_answers_only_what_caller_may_read(
    state: &ServerState,
    set: &str,
    owner_field: &str,
    text_field: &str,
) {
    let path = format!("/tdata/{set}");
    let alice_body = json!({
        "Id": "shared-key",
        owner_field: "alice",
        text_field: "alice private words",
    });
    let (status, body) = send(state, "POST", &path, "alice", Some(alice_body.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = send(
        state,
        "POST",
        &path,
        "bob",
        Some(json!({
            "Id": "shared-key",
            owner_field: "bob",
            text_field: "bob words",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(!body.contains("alice"), "{body}");
    assert!(!body.contains("fields"), "{body}");

    let (status, body) = send(
        state,
        "GET",
        &format!("{path}('shared-key')"),
        "alice",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stored["fields"][owner_field], "alice");
    assert_eq!(stored["fields"][text_field], "alice private words");

    let (status, body) = send(state, "POST", &path, "alice", Some(alice_body)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let repeated: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(repeated["fields"][text_field], "alice private words");
}

#[tokio::test]
async fn a_create_with_an_existing_key_answers_only_what_the_caller_may_read() {
    let (state, _store) = common::build_default_state(533, "create-existing-key");
    mark_verified(&state, "Order");
    state
        .authz
        .reload_tenant_policies("default", ORDER_OWNER_READS)
        .unwrap();
    assert_create_with_existing_key_answers_only_what_caller_may_read(
        &state,
        "Orders",
        "OrderNumber",
        "Notes",
    )
    .await;
}

#[tokio::test]
async fn a_data_only_create_with_an_existing_key_answers_only_what_the_caller_may_read() {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", dir.path().join("notes.db").display());
    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let specs = std::collections::BTreeMap::from([("Note".to_string(), NOTE_IOA.to_string())]);
    let mut state = ServerState::with_specs(
        ActorSystem::new("create-existing-key-data-only"),
        parse_csdl(NOTE_CSDL).unwrap(),
        NOTE_CSDL.to_string(),
        specs,
    )
    .unwrap();
    state.set_storage_stack(StorageStack::from_turso(store));
    mark_verified(&state, "Note");
    state
        .authz
        .reload_tenant_policies("default", NOTE_OWNER_READS)
        .unwrap();
    assert_create_with_existing_key_answers_only_what_caller_may_read(
        &state, "Notes", "Owner", "Body",
    )
    .await;
}
