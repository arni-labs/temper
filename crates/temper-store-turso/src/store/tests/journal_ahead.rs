//! Which live entities' journals are ahead of their catalog rows.

use temper_runtime::persistence::EventStore;

use crate::driver::params;

use super::super::TursoEventStore;
use super::{make_store, test_envelope, tombstone_envelope};

const ENTITY_TYPE: &str = "Payment";

async fn journal(store: &TursoEventStore, tenant: &str, entity_id: &str, events: usize) {
    let persistence_id = format!("{tenant}:{ENTITY_TYPE}:{entity_id}");
    for seq in 0..events {
        store
            .append(
                &persistence_id,
                seq as u64,
                &[test_envelope("Created", serde_json::json!({}))],
            )
            .await
            .expect("append event");
    }
}

async fn tombstone(store: &TursoEventStore, tenant: &str, entity_id: &str, after: u64) {
    store
        .append(
            &format!("{tenant}:{ENTITY_TYPE}:{entity_id}"),
            after,
            &[tombstone_envelope()],
        )
        .await
        .expect("append tombstone");
}

async fn project(store: &TursoEventStore, tenant: &str, entity_id: &str, seq: u64) {
    store
        .upsert_query_projection_with_state(
            tenant,
            ENTITY_TYPE,
            entity_id,
            "Pending",
            &serde_json::json!({ "PaymentId": entity_id }),
            &serde_json::json!({}),
            seq,
        )
        .await
        .expect("projection upsert");
}

async fn counters_sequence(store: &TursoEventStore, tenant: &str, entity_id: &str) -> Option<i64> {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT sequence_nr FROM entity_catalog_counters \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, ENTITY_TYPE, entity_id],
        )
        .await
        .expect("query counters row");
    rows.next()
        .await
        .expect("read counters row")
        .map(|row| row.get(0).expect("sequence_nr"))
}

#[tokio::test]
async fn lists_live_entities_with_no_catalog_row_or_a_row_behind_the_journal() {
    let store = make_store("journal-ahead-of-catalog").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());

    // Projected at its newest event.
    journal(&store, &tenant, "pay-current", 2).await;
    project(&store, &tenant, "pay-current", 2).await;
    // Journaled, never projected.
    journal(&store, &tenant, "pay-missing", 1).await;
    // A later event whose projection never landed.
    journal(&store, &tenant, "pay-behind", 2).await;
    project(&store, &tenant, "pay-behind", 1).await;
    // A later event whose projection matched the row, so only the counters
    // row moved to the newer sequence.
    journal(&store, &tenant, "pay-unchanged", 2).await;
    project(&store, &tenant, "pay-unchanged", 1).await;
    project(&store, &tenant, "pay-unchanged", 2).await;
    assert_eq!(
        counters_sequence(&store, &tenant, "pay-unchanged").await,
        Some(2),
        "fixture should carry the newer sequence in the counters row"
    );
    // Tombstoned, with and without a row left behind.
    journal(&store, &tenant, "pay-deleted", 1).await;
    tombstone(&store, &tenant, "pay-deleted", 1).await;
    journal(&store, &tenant, "pay-deleted-row", 1).await;
    project(&store, &tenant, "pay-deleted-row", 1).await;
    tombstone(&store, &tenant, "pay-deleted-row", 1).await;
    // Another tenant's record of the same type.
    let other_tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    journal(&store, &other_tenant, "pay-other-tenant", 1).await;

    let ids = store
        .entity_ids_ahead_of_catalog(&tenant, ENTITY_TYPE)
        .await
        .expect("list entities ahead of the catalog");
    assert_eq!(
        ids,
        vec!["pay-behind".to_string(), "pay-missing".to_string()]
    );
}
