//! A `Restrict` delete is answered from the query index, not by reading every
//! row that could reference the entity.
//!
//! The fixture CSDL declares `Payment.OrderId -> Order` with the default
//! `Restrict` delete policy. One server writes the rows; a second server opens
//! the same store, as a restarted process does, so no Payment is resident in
//! memory when an Order is deleted. Reading a Payment would start its actor, so
//! the server's live actor count shows whether the delete read them.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::projection_writes_fail::ProjectionWritesFail;
use common::{CSDL_XML, ORDER_IOA};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

/// `Payment` only needs to exist and carry the `OrderId` it was created with.
const PAYMENT_IOA: &str = r#"
[automaton]
name = "Payment"
states = ["Pending"]
initial = "Pending"
"#;

/// The same `Payment` before a spec change that starts indexing `OrderId`.
const PAYMENT_IOA_ORDER_ID_UNINDEXED: &str = r#"
[automaton]
name = "Payment"
states = ["Pending"]
initial = "Pending"

[[state]]
name = "OrderId"
type = "string"
initial = ""
query_indexed = false
"#;

/// Enough referencing rows that reading them shows up plainly in the actor count.
const REFERENCING_PAYMENTS: usize = 40;

/// More unprojected rows than any delete should read one by one.
const UNPROJECTED_PAYMENTS: usize = 250;

struct Store {
    db_path: std::path::PathBuf,
    db_url: String,
}

impl Store {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "temper-delete-reference-{name}-{}.db",
            uuid::Uuid::new_v4()
        ));
        let db_url = format!("file:{}", db_path.display());
        Self { db_path, db_url }
    }

    async fn open(&self) -> TursoEventStore {
        TursoEventStore::new(&self.db_url, None)
            .await
            .expect("open local turso db")
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
    }
}

fn server(name: &str, stack: StorageStack) -> ServerState {
    server_with(name, stack, PAYMENT_IOA)
}

fn server_with(name: &str, stack: StorageStack, payment_ioa: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA), ("Payment", payment_ioa)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(stack);
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Order", "Payment"] {
            registry.set_verification_status(
                &TenantId::default(),
                entity_type,
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
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy parses");
    state
}

/// A server whose writes reach the journal and the query projection.
async fn indexed_server(name: &str, store: &Store) -> ServerState {
    server(name, StorageStack::from_turso(store.open().await))
}

/// A server whose writes reach only the journal, as before a projection exists.
async fn journal_only_server(name: &str, store: &Store) -> ServerState {
    let mut stack = StorageStack::from_turso(store.open().await);
    stack.query_plane = None;
    server(name, stack)
}

/// A server whose projection writes fail while the returned switch is on.
async fn projection_failing_server(name: &str, store: &Store) -> (ServerState, Arc<AtomicBool>) {
    let (stack, failing) = ProjectionWritesFail::stack(store.open().await);
    (server(name, stack), failing)
}

/// The same store opened by a fresh process: ids indexed at boot, no actor resident.
async fn restarted_server(name: &str, store: &Store) -> ServerState {
    let state = indexed_server(name, store).await;
    state.populate_index_from_store(&TenantId::default()).await;
    state
}

async fn create(state: &ServerState, entity_type: &str, id: &str, fields: serde_json::Value) {
    state
        .get_or_create_tenant_entity(&TenantId::default(), entity_type, id, fields)
        .await
        .unwrap_or_else(|error| panic!("create {entity_type}('{id}'): {error}"));
}

async fn create_payments(state: &ServerState, prefix: &str, count: usize, order_id: &str) {
    for n in 0..count {
        create(
            state,
            "Payment",
            &format!("{prefix}-{n}"),
            serde_json::json!({ "OrderId": order_id }),
        )
        .await;
    }
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "delete-reference-test",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn delete_order(state: &ServerState, id: &str) -> (StatusCode, serde_json::Value) {
    let path = format!("/tdata/Orders('{id}')");
    send(state, Request::delete(path).body(Body::empty()).unwrap()).await
}

async fn get_order(state: &ServerState, id: &str) -> StatusCode {
    let path = format!("/tdata/Orders('{id}')");
    send(state, Request::get(path).body(Body::empty()).unwrap())
        .await
        .0
}

#[tokio::test]
async fn restrict_delete_is_answered_without_reading_the_referencing_rows() {
    let store = Store::new("indexed");
    let writer = indexed_server("delete-reference-writer", &store).await;
    create(&writer, "Order", "ord-referenced", serde_json::json!({})).await;
    create(&writer, "Order", "ord-unreferenced", serde_json::json!({})).await;
    create_payments(&writer, "pay", REFERENCING_PAYMENTS, "ord-referenced").await;

    let state = restarted_server("delete-reference-restarted", &store).await;
    let actors_before = state.active_actor_count();

    let (status, body) = delete_order(&state, "ord-unreferenced").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "unreferenced delete: {body}"
    );

    let (status, body) = delete_order(&state, "ord-referenced").await;
    assert_eq!(status, StatusCode::CONFLICT, "referenced delete: {body}");
    assert_eq!(body["error"]["details"]["type"], "relation_integrity");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("referenced by Payment('pay-"),
        "refusal names a referencing Payment: {message}"
    );

    // Only the two Orders themselves were loaded; none of the Payments.
    let actors_started = state.active_actor_count().saturating_sub(actors_before);
    assert!(
        actors_started <= 2,
        "the deletes started {actors_started} actors; reading the \
         {REFERENCING_PAYMENTS} Payments would start one each"
    );
    assert_eq!(
        get_order(&state, "ord-unreferenced").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_order(&state, "ord-referenced").await, StatusCode::OK);
}

#[tokio::test]
async fn restrict_delete_still_sees_a_reference_the_index_has_not_projected() {
    let store = Store::new("lagging");
    let writer = indexed_server("delete-reference-lag-writer", &store).await;
    create(&writer, "Order", "ord-lagging", serde_json::json!({})).await;
    create(&writer, "Order", "ord-free", serde_json::json!({})).await;
    create_payments(&writer, "pay", REFERENCING_PAYMENTS, "ord-elsewhere").await;

    // One Payment reaches the journal but not the projection yet.
    let lagging = journal_only_server("delete-reference-lag-journal", &store).await;
    create(
        &lagging,
        "Payment",
        "pay-unprojected",
        serde_json::json!({ "OrderId": "ord-lagging" }),
    )
    .await;

    let state = restarted_server("delete-reference-lag-restarted", &store).await;

    let (status, body) = delete_order(&state, "ord-lagging").await;
    assert_eq!(status, StatusCode::CONFLICT, "lagging reference: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("referenced by Payment('pay-unprojected')"),
        "refusal names the unprojected Payment: {message}"
    );

    let (status, body) = delete_order(&state, "ord-free").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "unreferenced delete: {body}"
    );
}

#[tokio::test]
async fn restrict_delete_refuses_when_the_index_cannot_vouch_for_the_rows() {
    let store = Store::new("unprojected");
    let writer = journal_only_server("delete-reference-unprojected-writer", &store).await;
    create(&writer, "Order", "ord-target", serde_json::json!({})).await;
    create_payments(&writer, "pay", UNPROJECTED_PAYMENTS, "ord-elsewhere").await;

    let state = restarted_server("delete-reference-unprojected-restarted", &store).await;
    let actors_before = state.active_actor_count();

    let (status, body) = delete_order(&state, "ord-target").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a delete the index cannot check is refused, not scanned: {body}"
    );
    assert_eq!(body["error"]["details"]["type"], "relation_unverified");

    let actors_started = state.active_actor_count().saturating_sub(actors_before);
    assert!(
        actors_started <= 1,
        "the refused delete started {actors_started} actors; it should read no Payment"
    );
    assert_eq!(get_order(&state, "ord-target").await, StatusCode::OK);
}

#[tokio::test]
async fn restrict_delete_reads_a_projected_row_whose_later_change_was_not_projected() {
    let store = Store::new("stale");
    let (state, projection_fails) =
        projection_failing_server("delete-reference-stale", &store).await;
    create(&state, "Order", "ord-target", serde_json::json!({})).await;
    create_payments(&state, "pay", REFERENCING_PAYMENTS, "ord-elsewhere").await;

    // pay-7 is projected pointing elsewhere. Its move to ord-target commits,
    // but the projection write fails, so the index still has the old value.
    projection_fails.store(true, Ordering::SeqCst);
    let moved = state
        .update_tenant_entity_fields(
            &TenantId::default(),
            "Payment",
            "pay-7",
            serde_json::json!({ "OrderId": "ord-target" }),
            false,
        )
        .await;
    assert!(
        moved
            .as_ref()
            .is_err_and(|error| error.contains("query projection write failed")),
        "the move commits and reports its failed projection: {moved:?}"
    );
    let committed = state
        .get_tenant_entity_state(&TenantId::default(), "Payment", "pay-7")
        .await
        .expect("read pay-7");
    assert_eq!(committed.state.fields["OrderId"], "ord-target");

    let (status, body) = delete_order(&state, "ord-target").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a reference the index missed refuses the delete: {body}"
    );
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("referenced by Payment('pay-7')"),
        "refusal names the moved Payment: {message}"
    );
    assert_eq!(get_order(&state, "ord-target").await, StatusCode::OK);

    // A restarted process sees the same journal and the same stale row.
    projection_fails.store(false, Ordering::SeqCst);
    let restarted = restarted_server("delete-reference-stale-restarted", &store).await;
    let (status, body) = delete_order(&restarted, "ord-target").await;
    assert_eq!(status, StatusCode::CONFLICT, "after restart: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("referenced by Payment('pay-7')"),
        "after restart the refusal names the moved Payment: {body}"
    );
}

#[tokio::test]
async fn restrict_delete_ignores_a_stale_row_that_no_longer_references_it() {
    let store = Store::new("stale-away");
    let (state, projection_fails) =
        projection_failing_server("delete-reference-stale-away", &store).await;
    create(&state, "Order", "ord-target", serde_json::json!({})).await;
    create_payments(&state, "pay", 3, "ord-target").await;

    // Every Payment moves away; the moves commit but none is projected, so
    // the index still says all three reference ord-target.
    projection_fails.store(true, Ordering::SeqCst);
    for n in 0..3 {
        let _ = state
            .update_tenant_entity_fields(
                &TenantId::default(),
                "Payment",
                &format!("pay-{n}"),
                serde_json::json!({ "OrderId": "ord-elsewhere" }),
                false,
            )
            .await;
    }
    projection_fails.store(false, Ordering::SeqCst);

    let (status, body) = delete_order(&state, "ord-target").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "stale index entries do not hold a delete the rows no longer block: {body}"
    );
    assert_eq!(get_order(&state, "ord-target").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn restrict_delete_sees_a_reference_written_under_a_fields_object() {
    let store = Store::new("wrapped");
    let writer = indexed_server("delete-reference-wrapped-writer", &store).await;
    create(&writer, "Order", "ord-wrapped", serde_json::json!({})).await;
    create_payments(&writer, "pay", REFERENCING_PAYMENTS, "ord-elsewhere").await;
    // The create check reads this reference inside the `fields` object, so
    // the delete must too, though the field index holds top-level fields only.
    create(
        &writer,
        "Payment",
        "pay-wrapped",
        serde_json::json!({ "fields": { "OrderId": "ord-wrapped" } }),
    )
    .await;

    let state = restarted_server("delete-reference-wrapped-restarted", &store).await;
    let actors_before = state.active_actor_count();
    let (status, body) = delete_order(&state, "ord-wrapped").await;
    assert_eq!(status, StatusCode::CONFLICT, "wrapped reference: {body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("referenced by Payment('pay-wrapped')"),
        "refusal names the wrapped Payment: {body}"
    );
    let actors_started = state.active_actor_count().saturating_sub(actors_before);
    assert!(
        actors_started <= 1,
        "the delete started {actors_started} actors; the wrapped reference is \
         read from its projection, not its actor"
    );
    assert_eq!(get_order(&state, "ord-wrapped").await, StatusCode::OK);
}

/// A spec change that starts indexing a field leaves the rows projected
/// before it with no index entry for that field, while their catalog entries
/// stay current. The restrict delete must still see such a row's reference,
/// and a `$filter` on the field must still find the row.
#[tokio::test]
async fn restrict_delete_sees_a_reference_projected_before_its_field_was_indexed() {
    let store = Store::new("newly-indexed");
    let writer = server_with(
        "delete-reference-newly-indexed-writer",
        StorageStack::from_turso(store.open().await),
        PAYMENT_IOA_ORDER_ID_UNINDEXED,
    );
    create(&writer, "Order", "ord-early", serde_json::json!({})).await;
    let early = serde_json::json!({ "OrderId": "ord-early" });
    create(&writer, "Payment", "pay-early", early).await;

    // The next spec indexes OrderId, and nothing has projected pay-early again.
    let state = restarted_server("delete-reference-newly-indexed-restarted", &store).await;
    let get_payment = Request::get("/tdata/Payments('pay-early')");
    let (status, payment) = send(&state, get_payment.body(Body::empty()).unwrap()).await;
    assert_eq!(
        (status, payment["fields"]["OrderId"].as_str()),
        (StatusCode::OK, Some("ord-early")),
        "pay-early holds the reference: {payment}"
    );

    let (deleted, body) = delete_order(&state, "ord-early").await;
    let filter = Request::get("/tdata/Payments?$filter=OrderId%20eq%20'ord-early'");
    let (_, listed) = send(&state, filter.body(Body::empty()).unwrap()).await;
    assert_eq!(
        (deleted, listed.to_string().contains("pay-early")),
        (StatusCode::CONFLICT, true),
        "the delete is refused for pay-early ({body}) and the filter finds it ({listed})"
    );
}

/// The same, after the restarted server's field-index backfill has run over
/// every record: the backfill re-projects pay-early under the spec that
/// indexes OrderId.
#[tokio::test]
async fn restrict_delete_sees_a_newly_indexed_reference_after_the_backfill() {
    let store = Store::new("newly-indexed-backfilled");
    let writer = server_with(
        "delete-reference-newly-indexed-backfill-writer",
        StorageStack::from_turso(store.open().await),
        PAYMENT_IOA_ORDER_ID_UNINDEXED,
    );
    create(&writer, "Order", "ord-early", serde_json::json!({})).await;
    let early = serde_json::json!({ "OrderId": "ord-early" });
    create(&writer, "Payment", "pay-early", early).await;

    let state = restarted_server("delete-reference-newly-indexed-backfilled", &store).await;
    state
        .populate_field_index_from_snapshots(&TenantId::default())
        .await;

    let (deleted, body) = delete_order(&state, "ord-early").await;
    let filter = Request::get("/tdata/Payments?$filter=OrderId%20eq%20'ord-early'");
    let (_, listed) = send(&state, filter.body(Body::empty()).unwrap()).await;
    assert_eq!(
        (deleted, listed.to_string().contains("pay-early")),
        (StatusCode::CONFLICT, true),
        "after the backfill, the delete is refused for pay-early ({body}) and the filter finds it ({listed})"
    );
}
