//! `GET /tdata/Set('key')/Property` reads one field of one record, whatever
//! its size.
//!
//! A whole-record read inlines a field kept out of line only up to its
//! hydration budget and leaves a larger one as a bare reference. The property
//! read is how a reader gets that field: authorized as a read of the record,
//! answered as `{"value": ...}` with the field's whole value.
//!
//! Fixture: a `Batch` with an `Items` list and a `Label`, on a Turso store,
//! which keeps a field larger than the inline limit out of line.

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

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Prop" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Batch">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Items" Type="Collection(Edm.String)"/>
        <Property Name="Label" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="PropService">
        <EntitySet Name="Batches" EntityType="Temper.Prop.Batch"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const BATCH_IOA: &str = r#"
[automaton]
name = "Batch"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "Fill"
kind = "input"
from = ["Draft"]
params = ["Items", "Label"]
"#;

const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// A list of one item whose JSON is 1.5 MiB: above both the whole read's
/// per-field budget (128 KiB) and its per-response budget (1 MiB), and below
/// the 2 MiB request body the write takes.
const LARGE_ITEMS_BYTES: usize = 3 * 512 * 1024;

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
    let mut state = ServerState::from_registry(ActorSystem::new("property-read"), registry);
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
            verified_at: "2026-11-03T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .expect("fixture policy should parse");
    state.data_dir = temp.path().join("data");
    state.set_storage_stack(StorageStack::from_turso(store));
    Fixture { state, _temp: temp }
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
                "property-read",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn get(state: &ServerState, uri: &str) -> impl Future<Output = (StatusCode, Value)> {
    send(state, Method::GET, uri, Value::Null)
}

/// Keep failure output short: a body may echo the large list.
fn short(value: &Value) -> String {
    value.to_string().chars().take(400).collect()
}

/// Create batch `id` and fill it with `items` and the label "nightly".
async fn filled_batch(state: &ServerState, id: &str, items: &Value) {
    let (status, body) = send(state, Method::POST, "/tdata/Batches", json!({"Id": id})).await;
    assert_eq!(status, StatusCode::CREATED, "create {id}: {}", short(&body));
    let (status, body) = send(
        state,
        Method::POST,
        &format!("/tdata/Batches('{id}')/Temper.Prop.Fill"),
        json!({"Items": items, "Label": "nightly"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fill {id}: {}", short(&body));
}

fn large_items() -> Value {
    // `["` + item + `"]`
    let items = json!(["x".repeat(LARGE_ITEMS_BYTES - 4)]);
    assert_eq!(serde_json::to_vec(&items).unwrap().len(), LARGE_ITEMS_BYTES);
    items
}

#[tokio::test]
async fn a_field_too_large_for_the_whole_read_reads_whole_on_its_own() {
    let fixture = fixture().await;
    let state = &fixture.state;
    let items = large_items();
    filled_batch(state, "big", &items).await;

    // The whole read leaves the list as its bare reference.
    let (status, record) = get(state, "/tdata/Batches('big')").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&record));
    assert!(
        record["Items"].get("__temper_blob_ref").is_some(),
        "the whole read inlines a 1.5 MiB field: {}",
        short(&record["Items"])
    );

    let (status, body) = get(state, "/tdata/Batches('big')/Items").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert_eq!(
        body["@odata.context"],
        "$metadata#Batches('big')/Items",
        "{}",
        short(&body)
    );
    assert!(
        body["value"] == items,
        "the list read whole: {}",
        short(&body)
    );
}

#[tokio::test]
async fn an_inline_field_reads_on_its_own() {
    let fixture = fixture().await;
    let state = &fixture.state;
    filled_batch(state, "small", &json!(["a", "b"])).await;

    let (status, body) = get(state, "/tdata/Batches('small')/Label").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert_eq!(body["value"], "nightly");
    let (status, body) = get(state, "/tdata/Batches('small')/Items").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert_eq!(body["value"], json!(["a", "b"]));
}

#[tokio::test]
async fn a_field_the_record_lacks_or_a_record_that_is_missing_is_not_found() {
    let fixture = fixture().await;
    let state = &fixture.state;
    filled_batch(state, "here", &json!(["a"])).await;

    let (status, body) = get(state, "/tdata/Batches('here')/Nothing").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", short(&body));
    let (status, body) = get(state, "/tdata/Batches('absent')/Label").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", short(&body));
}

#[tokio::test]
async fn a_reader_refused_the_record_is_refused_its_fields() {
    let fixture = fixture().await;
    let state = &fixture.state;
    filled_batch(state, "kept", &large_items()).await;
    state
        .authz
        .reload_tenant_policies(
            "default",
            &format!("{PERMIT_ALL}\nforbid(principal, action == Action::\"read\", resource);"),
        )
        .expect("refusing policy should parse");

    for field in ["Items", "Label"] {
        let (status, body) = get(state, &format!("/tdata/Batches('kept')/{field}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{field}: {}", short(&body));
        assert!(body.get("value").is_none(), "{field}: {}", short(&body));
    }
}
