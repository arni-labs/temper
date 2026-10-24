//! A write's record is checked on its values, whatever their size. A value
//! larger than the inline limit is stored out of line, behind a reference;
//! field invariants still read the value, both when this write brings it and
//! when an earlier write stored it and this write touches another field.
//!
//! Fixture: a `Batch` may run only with items
//! (`status == 'Running' => len(Items) > 0`), on a Turso store, which keeps
//! a field larger than the inline limit out of line.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

/// The largest serialized field the store keeps inline (ADR-0045).
const INLINE_LIMIT: usize = 131_072;

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Ovf" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Batch">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Items" Type="Collection(Edm.String)"/>
        <Property Name="Label" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="OvfService">
        <EntitySet Name="Batches" EntityType="Temper.Ovf.Batch"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const BATCH_IOA: &str = r#"
[automaton]
name = "Batch"
states = ["Draft", "Running"]
initial = "Draft"

[[action]]
name = "Fill"
kind = "input"
from = ["Draft"]
params = ["Items"]

[[action]]
name = "Start"
kind = "input"
from = ["Draft"]
to = "Running"
params = ["Items"]

[[action]]
name = "Run"
kind = "input"
from = ["Draft"]
to = "Running"

[[action]]
name = "Label"
kind = "input"
from = ["Draft", "Running"]
params = ["Label"]

[[field_invariant]]
name = "RunningBatchesHaveItems"
message = "a running batch has items"
assert = "status == 'Running' => len(Items) > 0"
"#;

struct Fixture {
    state: ServerState,
    _temp: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let db_url = format!("file:{}", temp.path().join("events.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create Turso test store");
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        "default",
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        &[("Batch", BATCH_IOA)],
        None,
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new("stored-record-overflow"), registry);
    state.data_dir = temp.path().join("data");
    state.set_storage_stack(StorageStack::from_turso(store));
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Batch",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-24T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("fixture policy should parse");
    Fixture { state, _temp: temp }
}

/// A one-item list whose JSON is exactly `len` bytes.
fn items_of_serialized_len(len: usize) -> Value {
    // `["` + item + `"]`
    let items = json!(["x".repeat(len - 4)]);
    assert_eq!(serde_json::to_vec(&items).unwrap().len(), len);
    items
}

async fn send(state: &ServerState, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "stored-record-overflow",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    // Keep failure output short: a body echoes the large list.
    let summary = Value::String(body.to_string().chars().take(400).collect());
    (status, summary)
}

async fn create_batch(state: &ServerState, id: &str) {
    let (status, body) = send(state, Method::POST, "/tdata/Batches", json!({"Id": id})).await;
    assert_eq!(status, StatusCode::CREATED, "create {id}: {body}");
}

async fn act(state: &ServerState, id: &str, action: &str, body: Value) -> (StatusCode, Value) {
    send(
        state,
        Method::POST,
        &format!("/tdata/Batches('{id}')/Temper.Ovf.{action}"),
        body,
    )
    .await
}

async fn stored(state: &ServerState, id: &str) -> (String, Value) {
    let response = state
        .get_tenant_entity_state(&TenantId::default(), "Batch", id)
        .await
        .expect("batch should be readable");
    (response.state.status, response.state.fields)
}

fn stored_out_of_line(value: &Value) -> bool {
    value.get("__temper_blob_ref").is_some()
}

#[tokio::test]
async fn an_action_bringing_a_list_on_either_side_of_the_inline_limit_is_checked_on_its_items() {
    let fixture = fixture().await;
    let state = &fixture.state;
    for (id, len) in [("at-limit", INLINE_LIMIT), ("over-limit", INLINE_LIMIT + 1)] {
        create_batch(state, id).await;
        let (status, body) = act(
            state,
            id,
            "Start",
            json!({"Items": items_of_serialized_len(len)}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{id} ({len} bytes): {body}");
        let (status, fields) = stored(state, id).await;
        assert_eq!(status, "Running", "{id}");
        assert_eq!(
            stored_out_of_line(&fields["Items"]),
            len > INLINE_LIMIT,
            "{id}: the list is stored out of line only above the limit"
        );
    }
}

#[tokio::test]
async fn an_action_bringing_an_empty_list_is_still_refused() {
    let fixture = fixture().await;
    let state = &fixture.state;
    create_batch(state, "empty").await;
    let (status, body) = act(state, "empty", "Start", json!({"Items": []})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored(state, "empty").await.0, "Draft");
}

#[tokio::test]
async fn a_write_touching_another_field_reads_the_list_an_earlier_write_stored_out_of_line() {
    let fixture = fixture().await;
    let state = &fixture.state;
    for (id, len) in [
        ("filled-at-limit", INLINE_LIMIT),
        ("filled-over-limit", INLINE_LIMIT + 1),
    ] {
        create_batch(state, id).await;
        let (status, body) = act(
            state,
            id,
            "Fill",
            json!({"Items": items_of_serialized_len(len)}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "fill {id}: {body}");
        let (_, fields) = stored(state, id).await;
        assert_eq!(
            stored_out_of_line(&fields["Items"]),
            len > INLINE_LIMIT,
            "{id}"
        );

        // The status change brings no items: the stored list is read.
        let (status, body) = act(state, id, "Run", json!({})).await;
        assert_eq!(status, StatusCode::OK, "run {id}: {body}");
        assert_eq!(stored(state, id).await.0, "Running", "{id}");

        // An action and a PATCH on another field re-check the stored list.
        let (status, body) = act(state, id, "Label", json!({"Label": "nightly"})).await;
        assert_eq!(status, StatusCode::OK, "label {id}: {body}");
        let (status, body) = send(
            state,
            Method::PATCH,
            &format!("/tdata/Batches('{id}')"),
            json!({"Label": "weekly"}),
        )
        .await;
        assert!(status.is_success(), "patch {id}: {status} {body}");
        assert_eq!(stored(state, id).await.1["Label"], "weekly", "{id}");
    }
}

#[tokio::test]
async fn running_without_items_is_still_refused() {
    let fixture = fixture().await;
    let state = &fixture.state;
    create_batch(state, "never-filled").await;
    let (status, body) = act(state, "never-filled", "Run", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    create_batch(state, "emptied").await;
    let (status, body) = act(state, "emptied", "Fill", json!({"Items": []})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = act(state, "emptied", "Run", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored(state, "emptied").await.0, "Draft");
}
