//! A create whose key already names an entity answers only with what the
//! caller may read: the stored entity when the caller may read it, and
//! `409 Conflict` with no entity data otherwise. Nothing is written either way.

mod common;

use axum::http::StatusCode;
use common::odata_requests::{mark_verified, send};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::state::DataOnlyCreate;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

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

/// A Note state on a scratch Turso store, with owner-only reads.
async fn note_state(dir: &tempfile::TempDir, system: &str) -> (ServerState, TursoEventStore) {
    let db_url = format!("file:{}", dir.path().join("notes.db").display());
    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    let specs = std::collections::BTreeMap::from([("Note".to_string(), NOTE_IOA.to_string())]);
    let mut state = ServerState::with_specs(
        ActorSystem::new(system),
        parse_csdl(NOTE_CSDL).unwrap(),
        NOTE_CSDL.to_string(),
        specs,
    )
    .unwrap();
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    mark_verified(&state, "Note");
    state
        .authz
        .reload_tenant_policies("default", NOTE_OWNER_READS)
        .unwrap();
    (state, store)
}

#[tokio::test]
async fn a_data_only_create_with_an_existing_key_answers_only_what_the_caller_may_read() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _store) = note_state(&dir, "create-existing-key-data-only").await;
    assert_create_with_existing_key_answers_only_what_caller_may_read(
        &state, "Notes", "Owner", "Body",
    )
    .await;
}

/// A create that finds the key's actor already started by another create
/// writes nothing and reports no new entity; the first create's fields stay.
#[tokio::test]
async fn a_create_whose_key_another_create_started_reports_no_new_entity() {
    let (state, _store) = common::build_default_state(535, "create-existing-key-held");
    let tenant = TenantId::default();
    state
        .get_or_spawn_tenant_actor_with_fields(
            &tenant,
            "Order",
            "held-key",
            json!({"OrderNumber": "alice"}),
        )
        .unwrap();

    let second = state
        .create_new_tenant_entity(&tenant, "Order", "held-key", json!({"OrderNumber": "bob"}))
        .await
        .unwrap();
    assert!(second.is_none(), "{second:?}");

    let stored = state
        .get_tenant_entity_state(&tenant, "Order", "held-key")
        .await
        .unwrap();
    assert_eq!(stored.state.fields["OrderNumber"], "alice");
}

/// A data-only create of a key the store already holds reports the key as
/// taken, not as a type that skips the data-only path.
#[tokio::test]
async fn a_data_only_create_of_a_stored_key_reports_the_key_taken() {
    let dir = tempfile::tempdir().unwrap();
    let (state, _store) = note_state(&dir, "create-existing-key-taken").await;
    let tenant = TenantId::default();
    let fields = |owner: &str| json!({"Id": "taken-key", "Owner": owner, "Body": "words"});

    let first = state
        .try_create_data_only_tenant_entity(&tenant, "Note", "taken-key", fields("alice"))
        .await
        .unwrap();
    assert!(matches!(first, DataOnlyCreate::Created(_)), "{first:?}");

    let second = state
        .try_create_data_only_tenant_entity(&tenant, "Note", "taken-key", fields("bob"))
        .await
        .unwrap();
    assert!(matches!(second, DataOnlyCreate::KeyTaken), "{second:?}");
}

/// A create stores the entity before its query projection. When the
/// projection is missing, the owner's repeated create writes it back, so
/// collection queries find the record again.
#[tokio::test]
async fn a_repeated_create_writes_back_a_missing_query_projection() {
    let dir = tempfile::tempdir().unwrap();
    let (state, store) = note_state(&dir, "create-existing-key-projection").await;
    let note = json!({"Id": "listed-key", "Owner": "alice", "Body": "alice words"});
    let owner_query = || {
        store.query_field_index(
            "default",
            "Note",
            "field_name = ?3 AND field_value = ?4",
            vec!["Owner".to_string(), "alice".to_string()],
        )
    };

    let (status, body) = send(&state, "POST", "/tdata/Notes", "alice", Some(note.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(owner_query().await.unwrap(), vec!["listed-key".to_string()]);

    store
        .remove_query_projection("default", "Note", "listed-key")
        .await
        .unwrap();
    assert!(owner_query().await.unwrap().is_empty());

    let (status, body) = send(&state, "POST", "/tdata/Notes", "alice", Some(note)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(owner_query().await.unwrap(), vec!["listed-key".to_string()]);
}
