//! Real PostgreSQL: a record's query-plane row and timer row commit with the
//! event that changes them, in its transaction, and nowhere else.

use sqlx::postgres::PgPoolOptions;
use temper_runtime::persistence::{
    CommitRows, EventMetadata, EventStore, PersistenceEnvelope, PersistenceError, ProjectionRow,
    StateTimerRecord, StateTimerRow,
};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

const ID: &str = "default:Job:job-1";

fn envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.into(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "entity-rows-test".into(),
        },
    }
}

fn timer(sequence_nr: u64) -> StateTimerRecord {
    StateTimerRecord {
        tenant: "default".into(),
        entity_type: "Job".into(),
        entity_id: "job-1".into(),
        state: "Busy".into(),
        arm_id: format!("Busy-{sequence_nr}"),
        sequence_nr,
        started_at: "2026-01-01T00:00:00+00:00".into(),
        params: "{}".into(),
        armed_by: "{}".into(),
    }
}

/// The catalog row's status, sequence and writing transaction, the field
/// index's rows, and the timer row's arm id and writing transaction.
#[derive(Debug, PartialEq)]
struct Rows {
    catalog: Option<(String, i64, String)>,
    field_index: Vec<(String, String)>,
    timer: Option<(String, String)>,
    event_writer: Option<String>,
}

async fn rows(pool: &sqlx::PgPool, schema: &PostgresSchema, sequence_nr: i64) -> Rows {
    let catalog = sqlx::query_as(&schema.qualify_sql(
        "SELECT status, sequence_nr, xmin::text FROM {schema}entity_catalog \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = 'job-1'",
    ))
    .fetch_optional(pool)
    .await
    .expect("catalog reads");
    let field_index = sqlx::query_as(&schema.qualify_sql(
        "SELECT field_name, field_value FROM {schema}entity_field_index \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = 'job-1' \
         ORDER BY field_name",
    ))
    .fetch_all(pool)
    .await
    .expect("field index reads");
    let timer = sqlx::query_as(&schema.qualify_sql(
        "SELECT arm_id, xmin::text FROM {schema}state_timers \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = 'job-1'",
    ))
    .fetch_optional(pool)
    .await
    .expect("timer reads");
    let event_writer = sqlx::query_scalar(&schema.qualify_sql(
        "SELECT xmin::text FROM {schema}events \
         WHERE tenant = 'default' AND entity_type = 'Job' AND entity_id = 'job-1' \
           AND sequence_nr = $1",
    ))
    .bind(sequence_nr)
    .fetch_optional(pool)
    .await
    .expect("events read");
    Rows {
        catalog,
        field_index,
        timer,
        event_writer,
    }
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn query_plane_and_timer_rows_commit_with_their_events() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    let name = format!("entity_rows_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(&name).expect("schema name");
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .expect("migrate");
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    let store_commits_rows = store.commits_entity_rows();
    let busy = serde_json::json!({ "Owner": "ana" });
    let state = serde_json::json!({ "status": "Busy" });

    // Entering the timed state: the catalog row, its field index and the
    // timer row land with the event, in its transaction.
    store
        .append_with_rows(
            ID,
            0,
            &[envelope("Hold")],
            CommitRows {
                projection: Some(ProjectionRow::Upsert {
                    status: "Busy",
                    fields: &busy,
                    state: &state,
                }),
                state_timer: Some(StateTimerRow::Arm(&timer(1))),
                ..CommitRows::default()
            },
        )
        .await
        .expect("hold appends");
    let held = rows(&pool, &schema, 1).await;

    // A refused append writes no row and removes none.
    let refused = store
        .append_with_rows(
            ID,
            0,
            &[envelope("Release")],
            CommitRows {
                projection: Some(ProjectionRow::Remove),
                state_timer: Some(StateTimerRow::End),
                ..CommitRows::default()
            },
        )
        .await;
    let after_refusal = rows(&pool, &schema, 1).await;

    // Leaving it: the timer row goes with the event.
    let open = serde_json::json!({ "Owner": "ana" });
    let open_state = serde_json::json!({ "status": "Open" });
    store
        .append_with_rows(
            ID,
            1,
            &[envelope("Release")],
            CommitRows {
                projection: Some(ProjectionRow::Upsert {
                    status: "Open",
                    fields: &open,
                    state: &open_state,
                }),
                state_timer: Some(StateTimerRow::End),
                ..CommitRows::default()
            },
        )
        .await
        .expect("release appends");
    let released = rows(&pool, &schema, 2).await;

    // A delete removes the catalog row and its field index with its event.
    store
        .append_with_rows(
            ID,
            2,
            &[envelope("Deleted")],
            CommitRows {
                projection: Some(ProjectionRow::Remove),
                ..CommitRows::default()
            },
        )
        .await
        .expect("delete appends");
    let deleted = rows(&pool, &schema, 3).await;

    sqlx::query(&format!("DROP SCHEMA \"{name}\" CASCADE"))
        .execute(&pool)
        .await
        .expect("drop schema");
    pool.close().await;

    let hold_writer = held.event_writer.clone().expect("the hold's event");
    assert_eq!(
        held.catalog,
        Some(("Busy".into(), 1, hold_writer.clone())),
        "the catalog row is the hold's commit"
    );
    assert_eq!(
        held.field_index,
        [("Owner".to_string(), "ana".to_string())],
        "the field index is written with it"
    );
    assert_eq!(
        held.timer,
        Some(("Busy-1".into(), hold_writer)),
        "the timer row is the hold's commit"
    );

    assert!(matches!(
        refused,
        Err(PersistenceError::ConcurrencyViolation { .. })
    ));
    assert_eq!(after_refusal, held, "a refused append leaves every row");

    let release_writer = released.event_writer.clone().expect("the release's event");
    assert_eq!(
        released.catalog,
        Some(("Open".into(), 2, release_writer)),
        "the catalog row is the release's commit"
    );
    assert_eq!(released.timer, None, "the release removed the timer row");

    assert_eq!(deleted.catalog, None, "the delete removed the catalog row");
    assert!(deleted.field_index.is_empty(), "and its field index");
    assert!(store_commits_rows, "the store says it commits entity rows");
}
