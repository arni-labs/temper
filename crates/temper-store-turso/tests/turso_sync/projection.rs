//! What a projection write costs a synced database's change log.

use temper_store_turso::QueryProjectionUpsert;

use super::support::*;

/// A projected state the size of a busy record's, with the counters every
/// action moves.
fn state(fields: &serde_json::Value, seq: u64) -> serde_json::Value {
    serde_json::json!({
        "entity_type": "Release",
        "entity_id": "rel-1",
        "status": "Observing",
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": fields,
        "events": [],
        "total_event_count": seq,
        "events_since_snapshot": seq,
        "last_snapshot_sequence_nr": 0,
        "sequence_nr": seq,
        "processed_idempotency_keys": {},
    })
}

#[tokio::test]
async fn an_action_that_changes_nothing_adds_nothing_to_the_change_log() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let store = server.open(&path).await;
    let copy = store.sync_copy().expect("copy").clone();
    let fields = serde_json::json!({
        "Title": "release",
        "Checks": "x".repeat(70_000),
    });

    store
        .upsert_query_projection_with_state(
            "default",
            "Release",
            "rel-1",
            "Observing",
            &fields,
            &state(&fields, 1),
            1,
        )
        .await
        .expect("first projection");
    push(&store).await;
    let before = copy.last_change().await.expect("newest change");

    for seq in 2..=11 {
        store
            .upsert_query_projection_with_state(
                "default",
                "Release",
                "rel-1",
                "Observing",
                &fields,
                &state(&fields, seq),
                seq,
            )
            .await
            .expect("unchanged projection");
        store
            .upsert_query_projections(
                "default",
                &[QueryProjectionUpsert {
                    entity_type: "Release".to_string(),
                    entity_id: "rel-1".to_string(),
                    status: "Observing".to_string(),
                    fields: fields.clone(),
                    state: state(&fields, seq),
                    indexed_fields: serde_json::json!({ "Title": "release" }),
                    sequence_nr: seq,
                    known_new: false,
                }],
            )
            .await
            .expect("unchanged batch projection");
    }

    assert_eq!(
        copy.last_change().await.expect("newest change"),
        before,
        "unchanged projections must not add change log rows"
    );
}
