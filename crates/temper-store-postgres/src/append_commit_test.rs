//! A move's rows commit with its events in one statement, or not at all.
//!
//! Gated on DATABASE_URL (skips without it, runs in CI); each test works in a
//! schema of its own.

use std::collections::BTreeMap;

use sqlx::PgPool;
use temper_runtime::persistence::{
    AppendProjection, CommitRows, EntityKeyRow, EntityVectorRow, EventMetadata, EventStore,
    PersistenceAppend, PersistenceEnvelope, PersistenceError, ProjectionRow, ScheduledActionRecord,
    StateTimerRecord, StateTimerRow,
};

use crate::migration::run_migrations_in_schema;
use crate::{PostgresEventStore, PostgresSchema};

const TENANT: &str = "t";
const PID: &str = "t:Panel:p1";

/// Assert the record's first move left a row in every table it writes
/// besides the journal's own, so a later "nothing changed" compares real rows.
/// On main the append wrote no catalog, field-index or timer rows.
fn assert_first_move_rows(snapshot: &BTreeMap<&'static str, Vec<String>>) {
    for table in [
        "entity_key_index",
        "entity_vector_index",
        "scheduled_actions",
        "entity_catalog",
        "entity_field_index",
        "state_timers",
    ] {
        assert!(
            !snapshot[table].is_empty(),
            "the first move wrote no {table} row"
        );
    }
}

/// Every table a move writes, in the order a snapshot reads them.
const TABLES: [&str; 9] = [
    "events",
    "event_segments",
    "entity_key_index",
    "entity_vector_index",
    "scheduled_actions",
    "entity_catalog",
    "entity_field_index",
    "entity_projection_delta",
    "state_timers",
];

fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "append-commit-test".to_string(),
        },
    }
}

fn tombstone() -> PersistenceEnvelope {
    envelope(
        "Deleted",
        serde_json::json!({
            "action": "Deleted", "from_status": "Open", "to_status": "Deleted",
            "params": {}, "kind": "tombstone"
        }),
    )
}

async fn isolated_store(url: &str) -> (PgPool, PostgresSchema, PostgresEventStore) {
    let pool = PgPool::connect(url).await.unwrap();
    let name = format!("append_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(name).unwrap();
    run_migrations_in_schema(&pool, &schema).await.unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    (pool, schema, store)
}

async fn drop_schema(pool: &PgPool, schema: &PostgresSchema) {
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.prefix().trim_end_matches('.')
    ))
    .execute(pool)
    .await
    .unwrap();
}

/// Every row of every table a move writes, as JSON text, so two snapshots
/// compare whole rows (timestamps and ids included).
async fn snapshot(pool: &PgPool, schema: &PostgresSchema) -> BTreeMap<&'static str, Vec<String>> {
    let mut rows = BTreeMap::new();
    for table in TABLES {
        let sql = format!(
            "SELECT row_to_json(r)::text FROM {}{table} r WHERE r.tenant = $1 ORDER BY 1",
            schema.prefix()
        );
        let found: Vec<(String,)> = sqlx::query_as(&sql)
            .bind(TENANT)
            .fetch_all(pool)
            .await
            .unwrap();
        rows.insert(table, found.into_iter().map(|(row,)| row).collect());
    }
    rows
}

async fn index_rows(store: &PostgresEventStore, pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(&store.schema().qualify_sql(
        "SELECT field_name, field_value FROM {schema}entity_field_index \
         WHERE tenant = 't' AND entity_id = 'p1' ORDER BY field_name COLLATE \"C\"",
    ))
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn count(store: &PostgresEventStore, pool: &PgPool, table: &'static str) -> i64 {
    let sql = format!(
        "SELECT COUNT(*)::bigint FROM {}{table} WHERE tenant = 't'",
        store.schema().prefix()
    );
    sqlx::query_scalar(&sql).fetch_one(pool).await.unwrap()
}

/// The record's rows besides its events: one key, one vector, one
/// scheduled action, a timer, and its projection.
struct Rows {
    key: Vec<EntityKeyRow>,
    vector: Vec<EntityVectorRow>,
    scheduled: Vec<ScheduledActionRecord>,
    timer: StateTimerRecord,
    fields: serde_json::Value,
    state: serde_json::Value,
}

impl Rows {
    fn at(sequence_nr: u64, fields: serde_json::Value) -> Self {
        Self {
            key: vec![EntityKeyRow {
                key_name: "slug".into(),
                key_hash: format!("slug-{sequence_nr}"),
            }],
            vector: vec![EntityVectorRow {
                decl_name: "body".into(),
                model_tag: "m1".into(),
                vector: vec![sequence_nr as f32, 1.0],
            }],
            scheduled: vec![ScheduledActionRecord {
                tenant: TENANT.into(),
                entity_type: "Panel".into(),
                entity_id: "p1".into(),
                arm_id: format!("{sequence_nr}-0"),
                action: "Remind".into(),
                due_at: "2026-11-03T00:00:00Z".into(),
                armed_by: "{}".into(),
            }],
            timer: StateTimerRecord {
                tenant: TENANT.into(),
                entity_type: "Panel".into(),
                entity_id: "p1".into(),
                state: "Open".into(),
                arm_id: format!("Open-{sequence_nr}"),
                sequence_nr,
                started_at: "2026-11-02T00:00:00Z".into(),
                params: "{}".into(),
                armed_by: "{}".into(),
            },
            state: serde_json::json!({"status": "Open", "sequence_nr": sequence_nr}),
            fields,
        }
    }

    fn commit(&self) -> CommitRows<'_> {
        CommitRows {
            key_rows: &self.key,
            vector_rows: &self.vector,
            reconcile_vectors: true,
            scheduled_actions: &self.scheduled,
            fired_scheduled_action: None,
            projection: Some(ProjectionRow::Upsert {
                status: "Open",
                fields: &self.fields,
                state: &self.state,
            }),
            state_timer: Some(StateTimerRow::Arm(&self.timer)),
        }
    }
}

/// One statement writes the event and every row with it, and the catalog and
/// field-index rows are the ones the projection writer writes for the same
/// record. On main the append ignored the projection and timer rows, so the
/// catalog, index and timer counts below were zero.
#[test]
fn append_writes_every_row_the_projection_writer_would() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        let rows = Rows::at(
            1,
            serde_json::json!({"Id": "p1", "app": "a1", "kind": "react"}),
        );
        let written = store
            .append_with_rows(
                PID,
                0,
                &[envelope("Created", serde_json::json!({}))],
                rows.commit(),
            )
            .await
            .unwrap();
        assert_eq!(written, 1);
        for table in TABLES {
            let expected = match table {
                // Id, app and kind are indexed.
                "entity_field_index" => 3,
                // A projected commit leaves no gap.
                "entity_projection_delta" => 0,
                _ => 1,
            };
            assert_eq!(count(&store, &pool, table).await, expected, "{table}");
        }

        // The same record projected by the writer the server used to call
        // after the commit, under another id, gives the same rows.
        store
            .upsert_query_projection_with_state(
                TENANT,
                "Panel",
                "p2",
                "Open",
                &rows.fields,
                &rows.state,
                1,
            )
            .await
            .unwrap();
        let catalog = |id: &'static str| {
            let pool = pool.clone();
            let sql = store.schema().qualify_sql(
                "SELECT status, fields::text, state::text, sequence_nr, projection_hash, \
                 projection_version, first_event_at FROM {schema}entity_catalog \
                 WHERE tenant = 't' AND entity_id = $1",
            );
            async move {
                let row: (
                    String,
                    String,
                    Option<String>,
                    i64,
                    String,
                    i32,
                    Option<String>,
                ) = sqlx::query_as(&sql)
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                row
            }
        };
        assert_eq!(catalog("p1").await, catalog("p2").await);
        let index_of = |id: &'static str| {
            let pool = pool.clone();
            let sql = store.schema().qualify_sql(
                "SELECT field_name, field_value, status FROM {schema}entity_field_index \
                 WHERE tenant = 't' AND entity_id = $1 ORDER BY field_name COLLATE \"C\"",
            );
            async move {
                let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(&sql)
                    .bind(id)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                rows
            }
        };
        assert_eq!(index_of("p1").await, index_of("p2").await);
        assert_eq!(
            count(&store, &pool, "entity_projection_delta").await,
            0,
            "a projected commit leaves no gap"
        );
        drop_schema(&pool, &schema).await;
    });
}

/// A stale expected sequence writes nothing: the statement reports the head
/// it found and every table is as it was.
#[test]
fn stale_expected_sequence_writes_no_rows() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        let first = Rows::at(1, serde_json::json!({"Id": "p1", "app": "a1"}));
        store
            .append_with_rows(
                PID,
                0,
                &[envelope("Created", serde_json::json!({}))],
                first.commit(),
            )
            .await
            .unwrap();
        let before = snapshot(&pool, &schema).await;
        assert_first_move_rows(&before);

        let stale = Rows::at(2, serde_json::json!({"Id": "p1", "app": "changed"}));
        let refused = store
            .append_with_rows(
                PID,
                0,
                &[envelope("Moved", serde_json::json!({}))],
                stale.commit(),
            )
            .await;
        assert!(
            matches!(
                refused,
                Err(PersistenceError::ConcurrencyViolation {
                    expected: 0,
                    actual: 1
                })
            ),
            "{refused:?}"
        );
        assert_eq!(snapshot(&pool, &schema).await, before);
        drop_schema(&pool, &schema).await;
    });
}

/// Two writers pass the sequence check together: the second meets the first
/// on the journal's unique key, and its statement, every row with it, aborts.
/// The rival here holds sequence 2 in an open transaction while the store's
/// statement waits on it.
#[test]
fn racing_writer_aborts_with_none_of_its_rows() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        let first = Rows::at(1, serde_json::json!({"Id": "p1", "app": "a1"}));
        store
            .append_with_rows(
                PID,
                0,
                &[envelope("Created", serde_json::json!({}))],
                first.commit(),
            )
            .await
            .unwrap();
        let before = snapshot(&pool, &schema).await;
        assert_first_move_rows(&before);

        let mut rival = pool.begin().await.unwrap();
        sqlx::query(&store.schema().qualify_sql(
            "INSERT INTO {schema}events \
             (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
             VALUES ('t', 'Panel', 'p1', 2, 'Rival', '{}', '{}')",
        ))
        .execute(&mut *rival)
        .await
        .unwrap();

        let racing = store.clone();
        let writer = tokio::spawn(async move {
            let rows = Rows::at(2, serde_json::json!({"Id": "p1", "app": "racer"}));
            racing
                .append_with_rows(
                    PID,
                    1,
                    &[envelope("Moved", serde_json::json!({}))],
                    rows.commit(),
                )
                .await
        });
        // Wait until the store's statement is blocked on the rival's row: it
        // has passed its sequence check and reached the journal insert. The
        // match is on the statement's opening, since pg_stat_activity keeps
        // only the first `track_activity_query_size` bytes of its text.
        let waiting = format!(
            "SELECT COUNT(*)::bigint FROM pg_stat_activity \
             WHERE wait_event_type = 'Lock' AND query LIKE 'WITH locked AS (%{}%'",
            schema.prefix().trim_end_matches('.').trim_matches('"')
        );
        let mut blocked = false;
        for _ in 0..500 {
            let waiters: i64 = sqlx::query_scalar(&waiting).fetch_one(&pool).await.unwrap();
            if waiters > 0 {
                blocked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(blocked, "the racing statement never waited on the rival");
        rival.commit().await.unwrap();

        let refused = writer.await.unwrap();
        assert!(
            matches!(
                refused,
                Err(PersistenceError::ConcurrencyViolation {
                    expected: 1,
                    actual: 2
                })
            ),
            "{refused:?}"
        );
        let mut after = snapshot(&pool, &schema).await;
        let events = after.remove("events").unwrap();
        assert_eq!(events.len(), 2, "the first event and the rival's");
        assert!(events.iter().any(|row| row.contains("\"Rival\"")));
        let mut before = before;
        before.remove("events");
        assert_eq!(after, before, "no row of the aborted writer remains");
        drop_schema(&pool, &schema).await;
    });
}

/// A move that drops one indexed field and changes another: the delete takes
/// the dropped name, the upsert the changed one, in the same statement.
#[test]
fn move_that_drops_one_field_and_changes_another() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        let first = Rows::at(
            1,
            serde_json::json!({"Id": "p1", "app": "a1", "kind": "react"}),
        );
        store
            .append_with_rows(
                PID,
                0,
                &[envelope("Created", serde_json::json!({}))],
                first.commit(),
            )
            .await
            .unwrap();
        let next = Rows::at(2, serde_json::json!({"Id": "p1", "app": "a2"}));
        store
            .append_with_rows(
                PID,
                1,
                &[envelope("Moved", serde_json::json!({}))],
                next.commit(),
            )
            .await
            .unwrap();
        assert_eq!(
            index_rows(&store, &pool).await,
            vec![("Id".into(), "p1".into()), ("app".into(), "a2".into())]
        );
        let (fields, sequence_nr): (serde_json::Value, i64) =
            sqlx::query_as(&store.schema().qualify_sql(
                "SELECT fields, sequence_nr FROM {schema}entity_catalog WHERE tenant = 't'",
            ))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fields, next.fields);
        assert_eq!(sequence_nr, 2);
        // The key, vector, scheduled action and timer moved with it.
        let key: (String, i64) = sqlx::query_as(&store.schema().qualify_sql(
            "SELECT key_hash, sequence_nr FROM {schema}entity_key_index WHERE tenant = 't'",
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(key, ("slug-2".into(), 2));
        assert_eq!(count(&store, &pool, "entity_vector_index").await, 1);
        assert_eq!(count(&store, &pool, "scheduled_actions").await, 2);
        let timer: String = sqlx::query_scalar(
            &store
                .schema()
                .qualify_sql("SELECT arm_id FROM {schema}state_timers WHERE tenant = 't'"),
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(timer, "Open-2");
        assert_eq!(count(&store, &pool, "entity_projection_delta").await, 0);
        drop_schema(&pool, &schema).await;
    });
}

/// A delete removes the catalog and index rows and the timer row, and leaves
/// a terminal mark so a late projection cannot bring the record back.
#[test]
fn tombstone_removes_rows_and_leaves_a_terminal_mark() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        let first = Rows::at(1, serde_json::json!({"Id": "p1", "app": "a1"}));
        store
            .append_with_rows(
                PID,
                0,
                &[envelope("Created", serde_json::json!({}))],
                first.commit(),
            )
            .await
            .unwrap();
        store
            .append_with_rows(
                PID,
                1,
                &[tombstone()],
                CommitRows {
                    projection: Some(ProjectionRow::Remove),
                    state_timer: Some(StateTimerRow::End),
                    reconcile_vectors: true,
                    ..CommitRows::default()
                },
            )
            .await
            .unwrap();
        for table in [
            "entity_catalog",
            "entity_field_index",
            "state_timers",
            "entity_vector_index",
        ] {
            assert_eq!(count(&store, &pool, table).await, 0, "{table}");
        }
        let mark: (i64, bool) = sqlx::query_as(&store.schema().qualify_sql(
            "SELECT sequence_nr, terminal FROM {schema}entity_projection_delta WHERE tenant = 't'",
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mark, (2, true));
        drop_schema(&pool, &schema).await;
    });
}

fn batched(
    persistence_id: &str,
    expected_sequence: u64,
    projection: Option<AppendProjection>,
) -> PersistenceAppend {
    PersistenceAppend {
        persistence_id: persistence_id.to_string(),
        expected_sequence,
        events: vec![envelope("Moved", serde_json::json!({}))],
        projection,
    }
}

fn kept(app: &str) -> Option<AppendProjection> {
    Some(AppendProjection::Upsert {
        status: "Kept".to_string(),
        fields: serde_json::json!({ "app": app }),
        state: serde_json::json!({}),
    })
}

/// Each record's catalog and field-index rows, by record.
async fn record_rows(
    store: &PostgresEventStore,
    pool: &PgPool,
) -> Vec<(String, i64, Option<String>)> {
    sqlx::query_as(&store.schema().qualify_sql(
        "SELECT c.entity_id, c.sequence_nr, f.field_value FROM {schema}entity_catalog c \
         LEFT JOIN {schema}entity_field_index f USING (tenant, entity_type, entity_id) \
         WHERE c.tenant = 't' ORDER BY c.entity_id COLLATE \"C\"",
    ))
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A batch (a composite move, a batch delete) commits every record's rows
/// with its events, as single appends do; a stale member writes nothing of
/// the batch. On main a batch appended only the events and marked each record
/// pending, and its rows were written after the commit.
#[test]
fn a_batch_commits_each_records_rows_with_its_events() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        store
            .append_batch(&[
                batched("t:Panel:p2", 0, kept("a2")),
                batched("t:Panel:p1", 0, kept("a1")),
            ])
            .await
            .unwrap();
        let rows = record_rows(&store, &pool).await;
        assert_eq!(
            rows,
            vec![
                ("p1".to_string(), 1, Some("a1".to_string())),
                ("p2".to_string(), 1, Some("a2".to_string())),
            ]
        );
        assert_eq!(count(&store, &pool, "entity_projection_delta").await, 0);

        // p1 is current, p2 is stale: nothing of the batch lands.
        let before = snapshot(&pool, &schema).await;
        let refused = store
            .append_batch(&[
                batched("t:Panel:p1", 1, kept("moved")),
                batched("t:Panel:p2", 0, kept("moved")),
            ])
            .await;
        assert!(
            matches!(
                refused,
                Err(PersistenceError::ConcurrencyViolation {
                    expected: 0,
                    actual: 1
                })
            ),
            "{refused:?}"
        );
        assert_eq!(snapshot(&pool, &schema).await, before);

        // A batch delete removes each record's rows in its commit.
        let mut deleted = batched("t:Panel:p1", 1, Some(AppendProjection::Remove));
        deleted.events = vec![tombstone()];
        store.append_batch(&[deleted]).await.unwrap();
        assert_eq!(
            record_rows(&store, &pool).await,
            vec![("p2".to_string(), 1, Some("a2".to_string()))]
        );
        drop_schema(&pool, &schema).await;
    });
}

#[path = "append_commit_test/race_test.rs"]
mod race;
