//! Seeded simulations of a create racing another request for the same key.
//!
//! Each seed drives one single-threaded execution against the simulated
//! journal. The create path's cooperative yield points (`buggify_yield`) take
//! a seeded number of yields, so the other request runs at a different point
//! of the create on each seed, and the same seed replays the same execution.
//! Seeds that broke an invariant on the code before the fix stay listed as
//! regression cases.

mod common;

use common::odata_requests::{mark_verified, send};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::{Value, json};
use temper_runtime::buggify::install_buggify_context;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow,
};
use temper_server::{ServerState, StorageStack};
use temper_store_sim::SimEventStore;

/// Anyone may create; only the principal an order's number names may read it.
const ORDER_OWNER_READS: &str = r#"
permit(principal, action == Action::"create", resource is Order);
permit(principal, action == Action::"read", resource is Order)
when { resource has OrderNumber && resource.OrderNumber == principal.id };
"#;

/// Seeds whose schedule gave a caller the other caller's order before racing
/// creates were fixed (50 of seeds 0..64 did; these are the first eight).
const RACE_REGRESSION_SEEDS: &[u64] = &[0, 1, 2, 4, 5, 7, 8, 10];

/// Seeds whose schedule put a deleted order back into the query projection
/// before the projection write checked for deletion (all of seeds 0..64 did;
/// these are the first eight).
const DELETE_REGRESSION_SEEDS: &[u64] = &[0, 1, 2, 3, 4, 5, 6, 7];

/// Seeds whose schedule broke a create of a deleted key: before such a create
/// answered 409, every seed revived the order (0-7 kept); once it answered
/// 409 but stopped the shared actor, 13 of 64 gave the other create a 500
/// (0, 1, 7, 12, 22, 23, 27, 29 kept).
const DELETED_KEY_REGRESSION_SEEDS: &[u64] = &[0, 1, 2, 3, 4, 5, 6, 7, 12, 22, 23, 27, 29];

/// Seeds whose schedule put an order back into the query projection after a
/// declared action moved it to `Deleted`, while the stores checked only for a
/// tombstone (14 of seeds 0..64 did; these are the first eight).
const DELETING_ACTION_REGRESSION_SEEDS: &[u64] = &[3, 6, 9, 12, 15, 18, 24, 30];

/// Seeds explored on every run besides the regression cases.
const EXPLORED_SEEDS: u64 = 64;

fn seeds(regressions: &'static [u64]) -> impl Iterator<Item = u64> {
    let explored = (0..EXPLORED_SEEDS).filter(|seed| !regressions.contains(seed));
    regressions.iter().copied().chain(explored)
}

fn order(owner: &str) -> Value {
    json!({
        "Id": "raced-key",
        "OrderNumber": owner,
        "Notes": format!("{owner} private words"),
    })
}

/// An Order that a declared action, not a delete, moves to `Deleted`.
const ORDER_DISCARDABLE_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Deleted"]
initial = "Draft"

[[action]]
name = "Discard"
kind = "input"
from = ["Draft"]
to = "Deleted"
"#;

/// Whether the journal leaves the entity deleted, as the stores read it: it
/// holds the tombstone, or its newest event moves the entity to `Deleted`.
fn journal_leaves_deleted(journal: &[PersistenceEnvelope]) -> bool {
    journal.iter().any(|event| event.event_type == "Deleted")
        || journal
            .last()
            .is_some_and(|event| event.payload["to_status"] == "Deleted")
}

/// The query projection as rows keyed by entity, with the stores' guards: a
/// write older than the row it would replace is refused, and so is any write
/// of an entity whose journal leaves it deleted.
struct ProjectionModel {
    rows: Mutex<BTreeMap<(String, String, String), u64>>,
    journal: SimEventStore,
}

impl ProjectionModel {
    fn new(journal: SimEventStore) -> Self {
        Self {
            rows: Mutex::default(),
            journal,
        }
    }

    fn holds(&self, entity_type: &str, entity_id: &str) -> bool {
        let key = (
            "default".to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        self.rows.lock().unwrap().contains_key(&key)
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for ProjectionModel {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        _status: &str,
        _fields: &Value,
        _state: &Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        let journal = self
            .journal
            .read_events(&format!("{tenant}:{entity_type}:{entity_id}"), 0)
            .await?;
        if journal_leaves_deleted(&journal) {
            return Ok(());
        }
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        let mut rows = self.rows.lock().unwrap();
        if rows.get(&key).is_none_or(|stored| *stored <= sequence_nr) {
            rows.insert(key, sequence_nr);
        }
        Ok(())
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        self.rows.lock().unwrap().remove(&key);
        Ok(())
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        Ok(None)
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

/// An Order state on the simulated journal with owner-only reads, and the
/// modelled query projection it writes to.
fn order_state(seed: u64, system: &str) -> (ServerState, Arc<ProjectionModel>) {
    order_state_with_spec(seed, system, common::ORDER_IOA)
}

/// [`order_state`] with the given Order spec.
fn order_state_with_spec(
    seed: u64,
    system: &str,
    order_ioa: &str,
) -> (ServerState, Arc<ProjectionModel>) {
    let journal = SimEventStore::no_faults(seed);
    let projection = Arc::new(ProjectionModel::new(journal.clone()));
    let mut state = common::build_single_tenant_state_with_store(
        journal.clone(),
        system,
        "default",
        &[("Order", order_ioa)],
    );
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(journal),
        None,
        None,
        None,
        None,
        Some(projection.clone()),
        None,
        None,
        None,
    ));
    mark_verified(&state, "Order");
    state
        .authz
        .reload_tenant_policies("default", ORDER_OWNER_READS)
        .unwrap();
    (state, projection)
}

/// Two principals create the same key at once. Exactly one create wins it,
/// every 201 carries the caller's own order, and the other caller gets 409
/// with none of the winner's data. `Err` names the broken invariant.
async fn racing_creates(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let _faults = install_buggify_context(seed);
    let (state, _projection) = order_state(seed, &format!("create-race-{seed}"));

    let (alice, bob) = tokio::join!(
        send(
            &state,
            "POST",
            "/tdata/Orders",
            "alice",
            Some(order("alice"))
        ),
        send(&state, "POST", "/tdata/Orders", "bob", Some(order("bob"))),
    );
    let mut created = 0;
    for (who, other, (status, body)) in [("alice", "bob", alice), ("bob", "alice", bob)] {
        if body.contains(other) {
            return Err(format!("{who} got {other}'s order: {status} {body}"));
        }
        match status {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {}
            other_status => return Err(format!("{who} got {other_status} {body}")),
        }
    }
    if created != 1 {
        return Err(format!("{created} creates won the key"));
    }
    Ok(())
}

/// The owner repeats a create while the order is deleted. However the two
/// interleave, the deleted order keeps no query projection row, so collection
/// queries do not list it again. `Err` names the broken invariant.
async fn repeated_create_racing_delete(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let (state, projection) = order_state(seed, &format!("create-delete-{seed}"));
    let (status, body) = send(
        &state,
        "POST",
        "/tdata/Orders",
        "alice",
        Some(order("alice")),
    )
    .await;
    if status != StatusCode::CREATED || !projection.holds("Order", "raced-key") {
        return Err(format!("the first create got {status} {body}"));
    }

    let _faults = install_buggify_context(seed);
    let tenant = TenantId::default();
    let (repeat, deleted) = tokio::join!(
        send(
            &state,
            "POST",
            "/tdata/Orders",
            "alice",
            Some(order("alice"))
        ),
        state.delete_tenant_entity(&tenant, "Order", "raced-key"),
    );
    match deleted {
        Ok(response) if response.success => {}
        other => return Err(format!("the delete failed: {other:?}")),
    }
    if projection.holds("Order", "raced-key") {
        return Err(format!(
            "the deleted order is back in the query projection; the repeat got {repeat:?}"
        ));
    }
    Ok(())
}

/// The owner repeats a create while a declared action moves the order to
/// `Deleted`. However the two interleave, the order keeps no query projection
/// row, so collection queries do not list it again. `Err` names the broken
/// invariant.
async fn repeated_create_racing_a_deleting_action(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let (state, projection) = order_state_with_spec(
        seed,
        &format!("create-discard-{seed}"),
        ORDER_DISCARDABLE_IOA,
    );
    let (status, body) = send(
        &state,
        "POST",
        "/tdata/Orders",
        "alice",
        Some(order("alice")),
    )
    .await;
    if status != StatusCode::CREATED || !projection.holds("Order", "raced-key") {
        return Err(format!("the first create got {status} {body}"));
    }

    let _faults = install_buggify_context(seed);
    let tenant = TenantId::default();
    let system = AgentContext::system();
    let (repeat, discarded) = tokio::join!(
        send(
            &state,
            "POST",
            "/tdata/Orders",
            "alice",
            Some(order("alice"))
        ),
        state.dispatch_tenant_action(&tenant, "Order", "raced-key", "Discard", json!({}), &system,),
    );
    match discarded {
        Ok(response) if response.success && response.state.is_deleted() => {}
        other => return Err(format!("the discard failed: {other:?}")),
    }
    if projection.holds("Order", "raced-key") {
        return Err(format!(
            "the discarded order is back in the query projection; the repeat got {repeat:?}"
        ));
    }
    Ok(())
}

/// An order is created and deleted, then its owner and another caller create
/// its key at once. Deletion is terminal: both get 409 with no body (never a
/// 500 from an actor the other create stopped), neither sees the deleted
/// order's fields, and the order stays out of reads, the
/// entity index and the query projection. `Err` names the broken invariant.
async fn creates_of_a_deleted_key(seed: u64) -> Result<(), String> {
    let _sim = install_deterministic_context(seed);
    let (state, projection) = order_state(seed, &format!("create-deleted-{seed}"));
    let tenant = TenantId::default();
    let (status, body) = send(
        &state,
        "POST",
        "/tdata/Orders",
        "alice",
        Some(order("alice")),
    )
    .await;
    if status != StatusCode::CREATED {
        return Err(format!("the first create got {status} {body}"));
    }
    match state
        .delete_tenant_entity(&tenant, "Order", "raced-key")
        .await
    {
        Ok(response) if response.success => {}
        other => return Err(format!("the delete failed: {other:?}")),
    }

    let _faults = install_buggify_context(seed);
    let (alice, bob) = tokio::join!(
        send(
            &state,
            "POST",
            "/tdata/Orders",
            "alice",
            Some(order("alice"))
        ),
        send(&state, "POST", "/tdata/Orders", "bob", Some(order("bob"))),
    );
    for (who, (status, body)) in [("alice", alice), ("bob", bob)] {
        if status != StatusCode::CONFLICT || body.contains("private words") {
            return Err(format!(
                "{who}'s create of the deleted key got {status} {body}"
            ));
        }
    }
    let (status, body) = send(&state, "GET", "/tdata/Orders('raced-key')", "alice", None).await;
    if status != StatusCode::NOT_FOUND {
        return Err(format!("a read of the deleted order got {status} {body}"));
    }
    if projection.holds("Order", "raced-key") {
        return Err("the deleted order is back in the query projection".to_string());
    }
    if state.entity_exists(&tenant, "Order", "raced-key") {
        return Err("the deleted order is back in the entity index".to_string());
    }
    Ok(())
}

#[tokio::test]
async fn racing_creates_of_one_key_answer_each_caller_only_with_its_own_record() {
    for seed in seeds(RACE_REGRESSION_SEEDS) {
        if let Err(violation) = racing_creates(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn a_repeated_create_racing_a_delete_leaves_the_deleted_record_out_of_queries() {
    for seed in seeds(DELETE_REGRESSION_SEEDS) {
        if let Err(violation) = repeated_create_racing_delete(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn a_repeated_create_racing_a_deleting_action_leaves_the_record_out_of_queries() {
    for seed in seeds(DELETING_ACTION_REGRESSION_SEEDS) {
        if let Err(violation) = repeated_create_racing_a_deleting_action(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[tokio::test]
async fn creates_of_a_deleted_key_answer_409_and_leave_it_deleted() {
    for seed in seeds(DELETED_KEY_REGRESSION_SEEDS) {
        if let Err(violation) = creates_of_a_deleted_key(seed).await {
            panic!("seed {seed}: {violation}");
        }
    }
}
