//! How many `entity_field_index` rows a move writes.
//!
//! A synced database's change log gains one row for every row a write inserts,
//! updates or deletes. The triggers below record those same row writes, so each
//! test reads what a move adds to that log from the field index. A move that
//! sets one field of a record writes one index row, whatever else it indexes.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{QueryProjectionUpsert, TursoEventStore, make_store};
use crate::driver::params;

const ENTITY_TYPE: &str = "Ticket";
/// Scalar fields on the test record; with the `Status` pseudo-field the record
/// has one more index row than this.
const FIELD_COUNT: usize = 20;

/// A record with `FIELD_COUNT` scalar fields, `Field00` to `Field19`, each
/// holding `value-<n>`, with `changes` applied on top (`null` removes one).
fn record_fields(changes: &[(&str, Value)]) -> Value {
    let mut fields = serde_json::Map::new();
    for n in 0..FIELD_COUNT {
        fields.insert(format!("Field{n:02}"), json!(format!("value-{n}")));
    }
    for (name, value) in changes {
        if value.is_null() {
            fields.remove(*name);
        } else {
            fields.insert((*name).to_string(), value.clone());
        }
    }
    Value::Object(fields)
}

fn record_state(entity_id: &str, status: &str, fields: &Value, seq: u64) -> Value {
    json!({
        "entity_type": ENTITY_TYPE,
        "entity_id": entity_id,
        "status": status,
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": fields,
        "events": [],
        "total_event_count": seq,
        "sequence_nr": seq,
    })
}

/// One move's projection, written as the server writes it: alone, or in a
/// batch of projections.
#[derive(Clone, Copy)]
enum Write {
    Single,
    Batched,
}

async fn project(
    store: &TursoEventStore,
    write: Write,
    [tenant, entity_id]: [&str; 2],
    status: &str,
    fields: &Value,
    seq: u64,
) {
    let state = record_state(entity_id, status, fields, seq);
    match write {
        Write::Single => store
            .upsert_query_projection_with_state(
                tenant,
                ENTITY_TYPE,
                entity_id,
                status,
                fields,
                &state,
                seq,
            )
            .await
            .expect("projection upsert"),
        Write::Batched => store
            .upsert_query_projections(
                tenant,
                &[QueryProjectionUpsert {
                    entity_type: ENTITY_TYPE.to_string(),
                    entity_id: entity_id.to_string(),
                    status: status.to_string(),
                    fields: fields.clone(),
                    state,
                    indexed_fields: fields.clone(),
                    sequence_nr: seq,
                    known_new: false,
                }],
            )
            .await
            .expect("batched projection upsert"),
    }
}

/// Record every row `entity_field_index` gains, changes or loses.
async fn log_field_index_writes(store: &TursoEventStore) {
    let conn = store.connection().await.expect("connection");
    conn.execute("CREATE TABLE field_index_writes (op TEXT NOT NULL)", ())
        .await
        .expect("create write log");
    for op in ["INSERT", "UPDATE", "DELETE"] {
        conn.execute(
            &format!(
                "CREATE TRIGGER field_index_{op} AFTER {op} ON entity_field_index BEGIN \
                   INSERT INTO field_index_writes VALUES ('{op}'); \
                 END"
            ),
            (),
        )
        .await
        .expect("create trigger");
    }
}

/// Row writes since the last call, by kind, so a failure shows what was
/// written.
async fn take_writes(store: &TursoEventStore) -> BTreeMap<String, i64> {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT op, count(*) FROM field_index_writes GROUP BY op",
            (),
        )
        .await
        .expect("read write log");
    let mut writes = BTreeMap::new();
    while let Some(row) = rows.next().await.expect("read write log row") {
        writes.insert(
            row.get::<String>(0).expect("op"),
            row.get::<i64>(1).expect("count"),
        );
    }
    drop(rows);
    conn.execute("DELETE FROM field_index_writes", ())
        .await
        .expect("clear write log");
    writes
}

fn total(writes: &BTreeMap<String, i64>) -> i64 {
    writes.values().sum()
}

/// The record's index rows as `field_name -> (field_value, status)`.
async fn index_rows(
    store: &TursoEventStore,
    [tenant, entity_id]: [&str; 2],
) -> BTreeMap<String, (Option<String>, Option<String>)> {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT field_name, field_value, status FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, ENTITY_TYPE, entity_id],
        )
        .await
        .expect("read index rows");
    let mut stored = BTreeMap::new();
    while let Some(row) = rows.next().await.expect("read index row") {
        stored.insert(
            row.get::<String>(0).expect("field_name"),
            (
                row.get::<Option<String>>(1).expect("field_value"),
                row.get::<Option<String>>(2).expect("status"),
            ),
        );
    }
    stored
}

/// The index rows a record with `fields` and `status` should have.
fn expected_rows(
    fields: &Value,
    status: &str,
) -> BTreeMap<String, (Option<String>, Option<String>)> {
    let mut expected = fields
        .as_object()
        .expect("fields object")
        .iter()
        .map(|(name, value)| {
            let text = value.as_str().map(str::to_string);
            (name.clone(), (text, Some(status.to_string())))
        })
        .collect::<BTreeMap<_, _>>();
    expected.insert(
        "Status".to_string(),
        (Some(status.to_string()), Some(status.to_string())),
    );
    expected
}

/// Each kind of move writes only the index rows it changes, and leaves the
/// rows exactly as a full rewrite would.
async fn a_move_writes_only_the_index_rows_it_changes(write: Write, name: &str) {
    let store = make_store(name).await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let ticket = [tenant.as_str(), "ticket-1"];
    log_field_index_writes(&store).await;

    let created = record_fields(&[]);
    project(&store, write, ticket, "Open", &created, 1).await;
    let writes = take_writes(&store).await;
    assert_eq!(
        total(&writes),
        FIELD_COUNT as i64 + 1,
        "the first projection writes one row per indexed field: {writes:?}"
    );
    assert_eq!(
        index_rows(&store, ticket).await,
        expected_rows(&created, "Open")
    );

    // (what the move does, its fields, its status, the index rows it writes)
    let moves = [
        (
            "sets one field",
            record_fields(&[("Field07", json!("edited"))]),
            "Open",
            1,
        ),
        (
            "sets three fields",
            record_fields(&[
                ("Field07", json!("edited")),
                ("Field01", json!("one")),
                ("Field02", json!("two")),
                ("Field03", json!("three")),
            ]),
            "Open",
            3,
        ),
        (
            "adds a field",
            record_fields(&[
                ("Field07", json!("edited")),
                ("Field01", json!("one")),
                ("Field02", json!("two")),
                ("Field03", json!("three")),
                ("Assignee", json!("ana")),
            ]),
            "Open",
            1,
        ),
        (
            "removes a field",
            record_fields(&[
                ("Field07", json!("edited")),
                ("Field01", json!("one")),
                ("Field02", json!("two")),
                ("Field03", json!("three")),
                ("Assignee", json!("ana")),
                ("Field19", Value::Null),
            ]),
            "Open",
            1,
        ),
    ];
    let mut seq = 1;
    let mut fields = created;
    for (what, next, status, expected) in moves {
        seq += 1;
        project(&store, write, ticket, status, &next, seq).await;
        let writes = take_writes(&store).await;
        assert_eq!(
            total(&writes),
            expected,
            "field-index rows written by a move that {what}: {writes:?}"
        );
        assert_eq!(
            index_rows(&store, ticket).await,
            expected_rows(&next, status),
            "index rows after a move that {what}"
        );
        fields = next;
    }

    // Every row carries the record's status, so a status change rewrites each
    // row once, and only once.
    seq += 1;
    project(&store, write, ticket, "Closed", &fields, seq).await;
    let writes = take_writes(&store).await;
    let rows = expected_rows(&fields, "Closed");
    assert_eq!(
        total(&writes),
        rows.len() as i64,
        "field-index rows written by a status change: {writes:?}"
    );
    assert_eq!(index_rows(&store, ticket).await, rows);
}

#[tokio::test]
async fn a_single_projection_writes_only_the_index_rows_it_changes() {
    a_move_writes_only_the_index_rows_it_changes(Write::Single, "field-index-writes-single").await;
}

#[tokio::test]
async fn a_batched_projection_writes_only_the_index_rows_it_changes() {
    a_move_writes_only_the_index_rows_it_changes(Write::Batched, "field-index-writes-batched")
        .await;
}
