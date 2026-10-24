//! An Order state on the simulated journal with a modelled query projection,
//! for simulations of creates racing other requests or failing part way.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use temper_runtime::persistence::{
    EventStore, PersistenceEnvelope, PersistenceError, journal_is_deleted,
};
use temper_server::storage::{
    BackendLabel, BoxedEventStore, QueryPlaneStore, QueryProjectionFieldsRow,
};
use temper_server::{ServerState, StorageStack};
use temper_store_sim::SimEventStore;

use super::odata_requests::mark_verified;

/// Whether the journal leaves the entity without a projection row, as the
/// stores read it: it holds the kernel's tombstone, or its newest event moves
/// the entity to the `Deleted` status.
fn journal_leaves_deleted(journal: &[PersistenceEnvelope]) -> bool {
    journal_is_deleted(journal)
        || journal
            .last()
            .is_some_and(|event| event.payload["to_status"] == "Deleted")
}

/// The query projection as rows keyed by entity, with the stores' guards: a
/// write older than the row it would replace is refused, and so is any write
/// of an entity whose journal leaves it deleted. A write can be made to fail
/// once, as a store that is briefly unavailable does.
pub struct ProjectionModel {
    rows: Mutex<BTreeMap<(String, String, String), u64>>,
    journal: SimEventStore,
    fail_next_write: AtomicBool,
}

impl ProjectionModel {
    fn new(journal: SimEventStore) -> Self {
        Self {
            rows: Mutex::default(),
            journal,
            fail_next_write: AtomicBool::new(false),
        }
    }

    /// Whether the projection holds a row for the entity.
    pub fn holds(&self, entity_type: &str, entity_id: &str) -> bool {
        let key = (
            "default".to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        self.rows.lock().unwrap().contains_key(&key)
    }

    /// Make the next projection write fail and write nothing.
    pub fn fail_next_write(&self) {
        self.fail_next_write.store(true, Ordering::SeqCst);
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
        if self.fail_next_write.swap(false, Ordering::SeqCst) {
            return Err(PersistenceError::Storage(
                "query projection unavailable".to_string(),
            ));
        }
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

/// An Order state with the given spec and Cedar policies on the simulated
/// journal, and the modelled query projection it writes to.
pub fn order_state(
    seed: u64,
    system: &str,
    order_ioa: &str,
    policies: &str,
) -> (ServerState, Arc<ProjectionModel>) {
    let journal = SimEventStore::no_faults(seed);
    let projection = Arc::new(ProjectionModel::new(journal.clone()));
    let mut state = super::build_single_tenant_state_with_store(
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
        .reload_tenant_policies("default", policies)
        .unwrap();
    (state, projection)
}
