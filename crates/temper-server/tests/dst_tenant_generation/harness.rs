//! One tenant, `acme`, with a timed and a self-scheduling record type, simulated event storage and a recording,
//! sequence-guarded query plane.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityResponse;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::{
    BackendLabel, BoxedEventStore, EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage,
    QueryPlaneStore, QueryProjectionFieldsRow,
};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Generation" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Bell">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Bells" EntityType="Temper.Generation.Bell"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A bell rings until its timer stops it 10 s later; an armed bell pings itself 10 s later.
const BELL_IOA: &str = r#"
[automaton]
name = "Bell"
states = ["Idle", "Ringing", "Rung", "Armed", "Pinged"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Ringing", "Rung", "Armed", "Pinged"]

[[action]]
name = "Touch"
kind = "input"
from = ["Idle", "Ringing", "Armed"]

[[action]]
name = "Ring"
kind = "input"
from = ["Idle"]
to = "Ringing"

[[action]]
name = "Stop"
kind = "internal"
from = ["Ringing"]
to = "Rung"

[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Armed"
effect = ["schedule('Ping', 10)"]

[[action]]
name = "Ping"
kind = "internal"
from = ["Armed"]
to = "Pinged"

[[state_timeout]]
state = "Ringing"
after_seconds = 10
on_timeout = "Stop"
"#;

/// The tenant every case deletes and creates again.
pub fn acme() -> TenantId {
    TenantId::new("acme")
}

/// How many of the tenant's actors are running (not halted).
pub fn live_actors(state: &ServerState) -> usize {
    state
        .actor_registry
        .read()
        .expect("actor registry")
        .iter()
        .filter(|(key, actor)| key.starts_with("acme:") && !actor.is_closed())
        .count()
}

/// The server, its simulated journal and its recording query plane.
pub struct Harness {
    pub state: ServerState,
    pub events: SimEventStore,
    pub rows: Arc<ProjectionRows>,
}

impl Harness {
    pub fn new(seed: u64) -> Self {
        let events = SimEventStore::no_faults(seed);
        let rows = Arc::new(ProjectionRows::default());
        let mut state = ServerState::from_registry(
            ActorSystem::new("dst-tenant-generation"),
            SpecRegistry::new(),
        );
        state.set_storage_stack(StorageStack::new(
            BackendLabel::Sim,
            BoxedEventStore::from_arc(Arc::new(events.clone())),
            None,
            None,
            None,
            None,
            Some(rows.clone() as Arc<dyn QueryPlaneStore>),
            None,
            None,
            None,
        ));
        let harness = Self {
            state,
            events,
            rows,
        };
        harness.register();
        harness
    }

    /// Create the tenant: register its specs.
    pub fn register(&self) {
        self.state
            .registry
            .write()
            .expect("registry")
            .register_tenant(
                "acme",
                parse_csdl(CSDL_XML).expect("CSDL parses"),
                CSDL_XML.to_string(),
                &[("Bell", BELL_IOA)],
            );
    }

    /// Remove the tenant's storage: its journal and its projection rows.
    pub fn remove_storage(&self) -> bool {
        self.rows.remove_tenant("acme");
        self.events.remove_tenant("acme")
    }

    /// Delete the tenant as `DELETE /api/tenants/acme` does.
    pub async fn delete(&self) {
        self.state
            .remove_tenant(&self.state.tenant_generations.stamp(&acme()), || async {
                Ok(self.remove_storage())
            })
            .await
            .expect("the removal finishes");
    }

    pub async fn dispatch(&self, id: &str, action: &str) -> Result<EntityResponse, String> {
        self.state
            .dispatch_tenant_action(
                &acme(),
                "Bell",
                id,
                action,
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
    }

    pub async fn status(&self, id: &str) -> String {
        self.state
            .get_tenant_entity_state(&acme(), "Bell", id)
            .await
            .expect("the bell exists")
            .state
            .status
    }
}

/// Projection rows keyed by (tenant, entity id): (status, sequence). A write with a sequence no newer than the row's
/// is ignored, as the real stores guard it.
#[derive(Default)]
pub struct ProjectionRows {
    rows: Mutex<BTreeMap<(String, String), (String, u64)>>,
    failures: AtomicUsize,
}

impl ProjectionRows {
    /// Fail the next `count` upserts, as a store that is slow to recover.
    pub fn fail_next(&self, count: usize) {
        self.failures.store(count, Ordering::SeqCst);
    }

    pub fn row(&self, id: &str) -> Option<(String, u64)> {
        self.rows
            .lock()
            .expect("rows")
            .get(&("acme".to_string(), id.to_string()))
            .cloned()
    }

    fn remove_tenant(&self, tenant: &str) {
        self.rows
            .lock()
            .expect("rows")
            .retain(|(owner, _), _| owner != tenant);
    }
}

#[async_trait]
impl QueryPlaneStore for ProjectionRows {
    async fn upsert_projection(
        &self,
        tenant: &str,
        _entity_type: &str,
        entity_id: &str,
        status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(PersistenceError::Storage("the store is recovering".into()));
        }
        let mut rows = self.rows.lock().expect("rows");
        let row = rows
            .entry((tenant.to_string(), entity_id.to_string()))
            .or_insert_with(|| (String::new(), 0));
        if sequence_nr > row.1 {
            *row = (status.to_string(), sequence_nr);
        }
        Ok(())
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        _entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.rows
            .lock()
            .expect("rows")
            .remove(&(tenant.to_string(), entity_id.to_string()));
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

    async fn query_field_index_page(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
        _order_by: &[QueryFieldIndexOrder],
        _skip: usize,
        _top: usize,
        _include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
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

    async fn load_entity_catalog_rows(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        Ok(None)
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}
