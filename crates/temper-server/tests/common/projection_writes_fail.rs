//! A query plane whose projection writes can be made to fail, as a committed
//! change whose projection write failed leaves the index behind its journal.
#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use temper_runtime::persistence::PersistenceError;
use temper_server::StorageStack;
use temper_server::storage::{
    CatalogRowFreshness, EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage,
    QueryPlaneStore, QueryProjectionFieldsRow, QueryProjectionUpsert,
};
use temper_store_turso::TursoEventStore;

/// The store's query plane, except that projection writes fail while
/// `failing` is set: reads still answer from what was projected before.
pub struct ProjectionWritesFail {
    store: TursoEventStore,
    failing: Arc<AtomicBool>,
}

impl ProjectionWritesFail {
    /// `store`'s storage stack with this query plane, and the switch that
    /// makes its projection writes fail while it is on.
    pub fn stack(store: TursoEventStore) -> (StorageStack, Arc<AtomicBool>) {
        let failing = Arc::new(AtomicBool::new(false));
        let mut stack = StorageStack::from_turso(store.clone());
        stack.query_plane = Some(Arc::new(Self {
            store,
            failing: failing.clone(),
        }));
        (stack, failing)
    }

    fn refuse(&self) -> Result<(), PersistenceError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(PersistenceError::Storage(
                "injected projection write failure".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for ProjectionWritesFail {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.refuse()?;
        QueryPlaneStore::upsert_projection(
            &self.store,
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
        )
        .await
    }

    async fn upsert_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        self.refuse()?;
        QueryPlaneStore::upsert_projections(&self.store, tenant, projections).await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.refuse()?;
        QueryPlaneStore::remove_projection(&self.store, tenant, entity_type, entity_id).await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        QueryPlaneStore::query_field_index(&self.store, tenant, entity_type, where_clause, params)
            .await
    }

    async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
        order_by: &[QueryFieldIndexOrder],
        skip: usize,
        top: usize,
        include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
        QueryPlaneStore::query_field_index_page(
            &self.store,
            tenant,
            entity_type,
            where_clause,
            params,
            order_by,
            skip,
            top,
            include_count,
        )
        .await
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        QueryPlaneStore::load_projection_fields_many(
            &self.store,
            tenant,
            entity_type,
            entity_ids,
            field_names,
        )
        .await
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        QueryPlaneStore::load_entity_catalog_rows(&self.store, tenant, entity_type, entity_ids)
            .await
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        QueryPlaneStore::projected_entity_counts_by_tenant(&self.store).await
    }

    async fn projection_catalog_freshness(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<Vec<CatalogRowFreshness>>, PersistenceError> {
        QueryPlaneStore::projection_catalog_freshness(&self.store, tenant, entity_type).await
    }
}
