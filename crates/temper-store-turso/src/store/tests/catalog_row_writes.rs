//! A record's catalog row holds each field once, and a move writes only the
//! columns it changes.
//!
//! Turso's change log keeps the whole row before and after every UPDATE, plus
//! the value of each column the UPDATE sets. The triggers below record those
//! same sizes, so each test reads what a move adds to that log.

use serde_json::{Value, json};

use super::{QueryProjectionUpsert, TursoEventStore, make_store};
use crate::driver::params;

const ENTITY_TYPE: &str = "Note";
const BODY_BYTES: usize = 100_000;

/// A note whose `Body` is large and whose other state is small.
fn note(entity_id: &str, title: &str, status: &str, sequence_nr: u64) -> (Value, Value) {
    let fields = json!({"Title": title, "Body": "b".repeat(BODY_BYTES)});
    let state = json!({
        "entity_type": ENTITY_TYPE,
        "entity_id": entity_id,
        "status": status,
        "item_count": 0,
        "counters": {"edits": sequence_nr},
        "booleans": {},
        "lists": {},
        "fields": fields,
        "events": [],
        "total_event_count": sequence_nr,
        "sequence_nr": sequence_nr,
    });
    (fields, state)
}

/// Record, for every UPDATE of `entity_catalog`, the size of the row before
/// and after it and the size of each large column it sets.
async fn log_catalog_writes(store: &TursoEventStore) {
    let conn = store.connection().await.expect("connection");
    conn.execute(
        "CREATE TABLE catalog_write_bytes (bytes INTEGER NOT NULL)",
        (),
    )
    .await
    .expect("create write log");
    for trigger in [
        "CREATE TRIGGER catalog_row_images AFTER UPDATE ON entity_catalog BEGIN \
           INSERT INTO catalog_write_bytes VALUES ( \
             length(old.fields) + ifnull(length(old.state), 0) \
             + length(new.fields) + ifnull(length(new.state), 0)); \
         END",
        "CREATE TRIGGER catalog_fields_set AFTER UPDATE OF fields ON entity_catalog BEGIN \
           INSERT INTO catalog_write_bytes VALUES (length(new.fields)); \
         END",
        "CREATE TRIGGER catalog_state_set AFTER UPDATE OF state ON entity_catalog BEGIN \
           INSERT INTO catalog_write_bytes VALUES (ifnull(length(new.state), 0)); \
         END",
    ] {
        conn.execute(trigger, ()).await.expect("create trigger");
    }
}

/// Bytes the change log would have gained since the last call.
async fn take_logged_bytes(store: &TursoEventStore) -> i64 {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query("SELECT ifnull(sum(bytes), 0) FROM catalog_write_bytes", ())
        .await
        .expect("read write log");
    let bytes = rows
        .next()
        .await
        .expect("read write log row")
        .expect("sum row")
        .get::<i64>(0)
        .expect("logged bytes");
    drop(rows);
    conn.execute("DELETE FROM catalog_write_bytes", ())
        .await
        .expect("clear write log");
    bytes
}

/// `(length(fields), length(state))` of one stored catalog row.
async fn stored_lengths(store: &TursoEventStore, tenant: &str, entity_id: &str) -> (i64, i64) {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT length(fields), ifnull(length(state), 0) FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, ENTITY_TYPE, entity_id],
        )
        .await
        .expect("read catalog row");
    let row = rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row exists");
    (
        row.get::<i64>(0).expect("fields length"),
        row.get::<i64>(1).expect("state length"),
    )
}

async fn upsert(
    store: &TursoEventStore,
    tenant: &str,
    entity_id: &str,
    fields: &Value,
    state: &Value,
    sequence_nr: u64,
) {
    store
        .upsert_query_projection_with_state(
            tenant,
            ENTITY_TYPE,
            entity_id,
            state["status"].as_str().expect("status"),
            fields,
            state,
            sequence_nr,
        )
        .await
        .expect("upsert projection");
}

async fn loaded_state(store: &TursoEventStore, tenant: &str, entity_id: &str) -> (Value, Value) {
    let rows = store
        .load_entity_catalog_rows(tenant, ENTITY_TYPE, &[entity_id.to_string()])
        .await
        .expect("load catalog row");
    assert_eq!(rows.len(), 1);
    let row = rows.into_iter().next().expect("one row");
    (row.fields, row.state.expect("catalog row has a state"))
}

fn assert_holds_body_once(lengths: (i64, i64)) {
    let (fields_len, state_len) = lengths;
    let body = BODY_BYTES as i64;
    assert!(fields_len >= body, "fields hold the body: {fields_len}");
    assert!(
        state_len < body / 10,
        "state must not hold a second copy of the fields: {state_len} bytes"
    );
}

#[tokio::test]
async fn a_move_writes_each_field_once_and_skips_columns_it_does_not_change() {
    let store = make_store("catalog-row-writes").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-large";
    let body = BODY_BYTES as i64;
    log_catalog_writes(&store).await;

    let (fields, created) = note(id, "Plan", "Draft", 1);
    upsert(&store, &tenant, id, &fields, &created, 1).await;
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    take_logged_bytes(&store).await;

    // A move that changes the status and counters but no field: the change
    // log keeps the row before and after, each with one copy of the body.
    let (_, moved) = note(id, "Plan", "Active", 2);
    upsert(&store, &tenant, id, &fields, &moved, 2).await;
    let logged = take_logged_bytes(&store).await;
    assert!(
        logged < 2 * body + body / 10,
        "a move that changes no field logged {logged} bytes for a {body}-byte body"
    );
    assert_eq!(
        loaded_state(&store, &tenant, id).await,
        (fields.clone(), moved.clone())
    );

    // The same projection written again, as a replay or read repair does.
    upsert(&store, &tenant, id, &fields, &moved, 2).await;
    assert_eq!(
        take_logged_bytes(&store).await,
        0,
        "a write that changes nothing writes nothing"
    );

    // A move that changes a small field rewrites the fields column once.
    let (edited_fields, edited) = note(id, "Plan v2", "Active", 3);
    upsert(&store, &tenant, id, &edited_fields, &edited, 3).await;
    let logged = take_logged_bytes(&store).await;
    assert!(
        logged < 3 * body + body / 10,
        "a move that changes one field logged {logged} bytes for a {body}-byte body"
    );
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    assert_eq!(
        loaded_state(&store, &tenant, id).await,
        (edited_fields, edited)
    );
}

#[tokio::test]
async fn a_row_that_holds_its_fields_twice_reads_unchanged_and_shrinks_on_its_next_move() {
    let store = make_store("catalog-row-legacy").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-legacy";
    let (fields, created) = note(id, "Plan", "Draft", 1);
    let conn = store.connection().await.expect("connection");
    conn.execute(
        "INSERT INTO entity_catalog \
         (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash) \
         VALUES (?1, ?2, ?3, 'Draft', ?4, ?5, '2026-01-01T00:00:00Z', 1, 2, 'written-before')",
        params![
            tenant.as_str(),
            ENTITY_TYPE,
            id,
            fields.to_string(),
            created.to_string()
        ],
    )
    .await
    .expect("insert a row that holds its fields twice");

    assert_eq!(
        loaded_state(&store, &tenant, id).await,
        (fields.clone(), created)
    );

    let (_, moved) = note(id, "Plan", "Active", 2);
    upsert(&store, &tenant, id, &fields, &moved, 2).await;
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    assert_eq!(loaded_state(&store, &tenant, id).await, (fields, moved));
}

#[tokio::test]
async fn fields_kept_out_of_the_query_plane_stay_in_the_stored_state() {
    let store = make_store("catalog-row-unindexed").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-unindexed";
    let (indexed, mut state) = note(id, "Plan", "Draft", 1);
    state["fields"]["Heartbeat"] = json!("beat-1");

    upsert(&store, &tenant, id, &indexed, &state, 1).await;

    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    let (loaded_fields, loaded) = loaded_state(&store, &tenant, id).await;
    assert_eq!(loaded_fields, indexed);
    assert_eq!(loaded, state);
}

#[tokio::test]
async fn a_batched_move_writes_each_field_once() {
    let store = make_store("catalog-row-batch").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-batch";
    let body = BODY_BYTES as i64;
    log_catalog_writes(&store).await;
    let batch = |state: &Value, sequence_nr: u64, known_new: bool| QueryProjectionUpsert {
        entity_type: ENTITY_TYPE.to_string(),
        entity_id: id.to_string(),
        status: state["status"].as_str().expect("status").to_string(),
        fields: state["fields"].clone(),
        state: state.clone(),
        indexed_fields: json!({"Title": state["fields"]["Title"].clone()}),
        sequence_nr,
        known_new,
    };

    let (_, created) = note(id, "Plan", "Draft", 1);
    store
        .upsert_query_projections(&tenant, &[batch(&created, 1, true)])
        .await
        .expect("batched create");
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    take_logged_bytes(&store).await;

    let (fields, moved) = note(id, "Plan", "Active", 2);
    store
        .upsert_query_projections(&tenant, &[batch(&moved, 2, false)])
        .await
        .expect("batched move");
    let logged = take_logged_bytes(&store).await;
    assert!(
        logged < 2 * body + body / 10,
        "a batched move that changes no field logged {logged} bytes for a {body}-byte body"
    );
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    assert_eq!(loaded_state(&store, &tenant, id).await, (fields, moved));

    let title_matches = store
        .query_field_index(
            &tenant,
            ENTITY_TYPE,
            "field_name = ?3 AND field_value = ?4",
            vec!["Title".to_string(), "Plan".to_string()],
        )
        .await
        .expect("query title");
    assert_eq!(title_matches, vec![id.to_string()]);
}
