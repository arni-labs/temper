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
        .expect("create");
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
