//! Point reads after a write (ADR-0148): projection writes are queued, so the
//! catalog row can be one write behind the commit this server just
//! acknowledged. A point read must not serve that row.

use super::dst_projection_lag::{SimQueryPlane, sim_state};
use super::*;
use crate::odata::read_support::try_load_entity_body_from_catalog;
use crate::storage::QueryPlaneStore;
use std::sync::Arc;

async fn project(qp: &SimQueryPlane, entity_id: &str, sequence_nr: u64) {
    qp.upsert_projection(
        TenantId::default().as_str(),
        "Order",
        entity_id,
        "Draft",
        &serde_json::json!({}),
        &serde_json::json!({ "status": "Draft", "sequence_nr": sequence_nr }),
        sequence_nr,
    )
    .await
    .expect("project");
}

#[tokio::test]
async fn a_point_read_does_not_serve_a_projection_behind_this_servers_commit() {
    let qp = Arc::new(SimQueryPlane::default());
    let (state, _events) = sim_state(11, qp.clone());
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("point-read-freshness");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o1",
            "Create",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("add item");
    let added = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o1",
            "AddItem",
            serde_json::json!({ "ProductId": "p1", "Quantity": 1 }),
            &agent_ctx,
        )
        .await
        .expect("add item");
    assert!(added.success, "{:?}", added.error);
    let committed = added.state.sequence_nr;
    assert!(committed > 1);

    project(&qp, "o1", committed - 1).await;
    assert!(
        try_load_entity_body_from_catalog(&state, &tenant, "Order", "Orders", "o1", true)
            .await
            .is_none(),
        "a row one write behind the actor must not be served"
    );

    project(&qp, "o1", committed).await;
    assert!(
        try_load_entity_body_from_catalog(&state, &tenant, "Order", "Orders", "o1", true)
            .await
            .is_some(),
        "a current row is served from the catalog"
    );
}

#[tokio::test]
async fn a_point_read_of_an_entity_not_in_memory_uses_the_projection() {
    let qp = Arc::new(SimQueryPlane::default());
    let (state, _events) = sim_state(12, qp.clone());
    let tenant = TenantId::default();
    project(&qp, "cold", 3).await;

    assert!(
        try_load_entity_body_from_catalog(&state, &tenant, "Order", "Orders", "cold", true)
            .await
            .is_some()
    );
    assert!(
        state
            .resident_entity_sequence(&tenant, "Order", "cold")
            .await
            .is_none(),
        "checking freshness must not spawn an actor"
    );
}

/// A transition with no reactions, integrations or timers is projected before
/// dispatch returns, so collection reads (`$filter`) see it at once too.
#[tokio::test]
async fn an_ordinary_transition_is_projected_before_dispatch_returns() {
    let qp = Arc::new(SimQueryPlane::default());
    let (state, _events) = sim_state(13, qp.clone());
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("projection-before-response");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o2",
            "Create",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("add item");
    let added = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o2",
            "AddItem",
            serde_json::json!({ "ProductId": "p1", "Quantity": 1 }),
            &agent_ctx,
        )
        .await
        .expect("add item");
    assert!(added.success, "{:?}", added.error);

    let rows = qp
        .load_entity_catalog_rows(tenant.as_str(), "Order", &["o2".to_string()])
        .await
        .expect("read catalog")
        .expect("catalog rows");
    assert_eq!(
        rows.first().map(|row| row.sequence_nr),
        Some(added.state.sequence_nr),
        "the projection must hold the committed sequence when dispatch returns"
    );
}

/// Fails the first projection write, then behaves like `SimQueryPlane`.
#[derive(Default)]
struct FlakyQueryPlane {
    inner: SimQueryPlane,
    failed_once: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl QueryPlaneStore for FlakyQueryPlane {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), temper_runtime::persistence::PersistenceError> {
        if !self
            .failed_once
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(temper_runtime::persistence::PersistenceError::Storage(
                "injected projection outage".to_string(),
            ));
        }
        self.inner
            .upsert_projection(
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

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), temper_runtime::persistence::PersistenceError> {
        self.inner
            .remove_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, temper_runtime::persistence::PersistenceError> {
        self.inner
            .query_field_index(tenant, entity_type, where_clause, params)
            .await
    }

    async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
        order_by: &[crate::storage::QueryFieldIndexOrder],
        skip: usize,
        top: usize,
        include_count: bool,
    ) -> Result<
        Option<crate::storage::QueryFieldIndexPage>,
        temper_runtime::persistence::PersistenceError,
    > {
        self.inner
            .query_field_index_page(
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
    ) -> Result<
        Option<Vec<crate::storage::QueryProjectionFieldsRow>>,
        temper_runtime::persistence::PersistenceError,
    > {
        self.inner
            .load_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<
        Option<Vec<crate::storage::EntityCatalogRow>>,
        temper_runtime::persistence::PersistenceError,
    > {
        self.inner
            .load_entity_catalog_rows(tenant, entity_type, entity_ids)
            .await
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, temper_runtime::persistence::PersistenceError> {
        self.inner.projected_entity_counts_by_tenant().await
    }
}

/// A failed projection write is reported, and the committed state is handed to
/// the retrying queue, so reads catch up once the store recovers.
#[tokio::test]
async fn a_failed_projection_write_is_retried_through_the_queue() {
    let qp = Arc::new(FlakyQueryPlane::default());
    let events =
        crate::storage::BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(14));
    let mut state = build_order_state("flaky-projection");
    state.set_storage_stack(crate::storage::StorageStack::new(
        crate::storage::BackendLabel::Sim,
        events,
        None,
        None,
        None,
        None,
        Some(qp.clone()),
        None,
        None,
        None,
    ));
    state.transition_tables = Arc::new(
        [(
            "Order".to_string(),
            Arc::new(temper_jit::table::TransitionTable::from_ioa_source(
                ORDER_IOA,
            )),
        )]
        .into_iter()
        .collect(),
    );
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("flaky-projection");
    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o3",
            "AddItem",
            serde_json::json!({ "ProductId": "p1", "Quantity": 1 }),
            &agent_ctx,
        )
        .await
        .expect("add item");
    assert!(
        !created.success
            && created
                .error
                .as_deref()
                .is_some_and(|error| error.contains("query projection failed")),
        "the projection outage is reported in the response: {:?}",
        created.error
    );

    let committed = created.state.sequence_nr;
    for _ in 0..200 {
        let rows = qp
            .load_entity_catalog_rows(tenant.as_str(), "Order", &["o3".to_string()])
            .await
            .expect("read catalog")
            .expect("catalog rows");
        if rows.first().is_some_and(|row| row.sequence_nr == committed) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("the queued retry never projected the committed state");
}
