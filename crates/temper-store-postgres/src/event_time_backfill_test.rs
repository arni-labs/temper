//! ADR-0187 "Backfill" on Postgres: the cases the Turso store's tests cover
//! (`temper-store-turso/src/store/tests/event_time_backfill.rs`), in an
//! isolated schema, gated on DATABASE_URL.

use super::*;
use crate::{PostgresSchema, migration::run_migrations_in_schema};
use serde_json::json;
use sqlx::PgPool;
use temper_runtime::persistence::{
    QueryFieldIndexOrder, QueryFieldIndexOrderDirection, QueryFieldIndexPageQuery,
};

const TENANT: &str = "app";
const TYPE: &str = "Ask";

async fn fresh_store(pool: &PgPool) -> (PostgresEventStore, PostgresSchema) {
    let schema = PostgresSchema::new(format!("evt_{}", uuid::Uuid::new_v4().simple())).unwrap();
    run_migrations_in_schema(pool, &schema).await.unwrap();
    (
        PostgresEventStore::with_schema(pool.clone(), schema.clone()),
        schema,
    )
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

/// A record made in October and not moved since: no times in its catalog
/// row unless given, its journal's `(sequence, time)`s, and a snapshot.
async fn seed(
    store: &PostgresEventStore,
    id: &str,
    events: &[(i64, &str)],
    times: (Option<&str>, Option<&str>),
    snapshot: Option<i64>,
) {
    let sequence = events.iter().map(|event| event.0).max().unwrap_or(1);
    let mut state = json!({
        "entity_type": TYPE, "entity_id": id, "status": "Open",
        "sequence_nr": sequence, "events": [],
    });
    if let Some(last) = times.1 {
        state["last_event_at"] = json!(last);
    }
    crate::dbm::postgres_query!(&store.schema,
        "INSERT INTO {schema}entity_catalog (tenant, entity_type, entity_id, status, fields, state, sequence_nr, first_event_at) \
         VALUES ($1, $2, $3, 'Open', '{}', $4, $5, $6)")
        .bind(TENANT).bind(TYPE).bind(id).bind(&state).bind(sequence).bind(times.0)
        .execute(store.pool()).await.unwrap();
    for (sequence, at) in events {
        let stamp = json!({ "timestamp": at });
        crate::dbm::postgres_query!(&store.schema,
            "INSERT INTO {schema}events (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
             VALUES ($1, $2, $3, $4, 'Moved', $5, $5)")
            .bind(TENANT).bind(TYPE).bind(id).bind(*sequence).bind(&stamp)
            .execute(store.pool()).await.unwrap();
    }
    if let Some(snapshot) = snapshot {
        crate::dbm::postgres_query!(&store.schema,
            "INSERT INTO {schema}snapshots (tenant, entity_type, entity_id, sequence_nr, state) VALUES ($1, $2, $3, $4, '\\x00')")
            .bind(TENANT).bind(TYPE).bind(id).bind(snapshot)
            .execute(store.pool()).await.unwrap();
    }
}

async fn by_last_event(store: &PostgresEventStore, descending: bool) -> Vec<String> {
    let page = QueryFieldIndexPageQuery {
        where_clause: "TRUE".to_string(),
        order_by: vec![QueryFieldIndexOrder {
            field_name: "last_event_at".to_string(),
            direction: if descending {
                QueryFieldIndexOrderDirection::Desc
            } else {
                QueryFieldIndexOrderDirection::Asc
            },
        }],
        top: 10,
        ..QueryFieldIndexPageQuery::default()
    };
    store
        .query_field_index_page(TENANT, TYPE, &page)
        .await
        .unwrap()
        .0
}

/// Every catalog row, whole, in key order.
async fn catalog(store: &PostgresEventStore) -> Vec<String> {
    crate::dbm::postgres_query_scalar!(&store.schema,
        "SELECT entity_id || '|' || status || '|' || COALESCE(state::text, '') || '|' || COALESCE(first_event_at, '') \
             || '|' || projection_hash || '|' || updated_at::text || '|' || sequence_nr \
         FROM {schema}entity_catalog ORDER BY tenant, entity_type, entity_id")
        .fetch_all(store.pool()).await.unwrap()
}

async fn times(store: &PostgresEventStore, id: &str) -> (Option<String>, Option<String>) {
    crate::dbm::postgres_query_as!(
        &store.schema,
        "SELECT first_event_at, state ->> 'last_event_at' FROM {schema}entity_catalog \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3"
    )
    .bind(TENANT)
    .bind(TYPE)
    .bind(id)
    .fetch_one(store.pool())
    .await
    .unwrap()
}

async fn reset_cursor(store: &PostgresEventStore) {
    crate::dbm::postgres_query!(&store.schema, "DELETE FROM {schema}event_time_backfill")
        .execute(store.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn an_october_record_sorts_by_its_journal_times_once_filled_on_postgres() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let pool = PgPool::connect(&url).await.unwrap();
    let (store, schema) = fresh_store(&pool).await;
    store
        .upsert_query_projection_with_state(
            TENANT,
            TYPE,
            "yesterday",
            "Open",
            &json!({}),
            &json!({
                "entity_type": TYPE, "entity_id": "yesterday", "status": "Open",
                "sequence_nr": 1, "events": [],
                "first_event_at": "2026-11-18T09:00:00.000000000Z",
                "last_event_at": "2026-11-18T09:00:00Z",
            }),
            1,
        )
        .await
        .unwrap();
    let journal = [
        (1, "2026-10-05T09:00:00.5+02:00"),
        (2, "2026-10-06T08:00:00Z"),
    ];
    seed(&store, "october", &journal, (None, None), None).await;
    seed(
        &store,
        "set",
        &journal,
        (
            Some("2026-09-01T00:00:00.000000000Z"),
            Some("2026-09-02T00:00:00Z"),
        ),
        None,
    )
    .await;
    seed(&store, "empty", &[], (None, None), None).await;
    seed(
        &store,
        "lost",
        &[(2, "2026-10-06T08:00:00Z")],
        (None, None),
        None,
    )
    .await;
    seed(&store, "snapshotted", &journal, (None, None), Some(2)).await;

    let newest = by_last_event(&store, true).await;
    assert_eq!(&newest[..1], ["empty"], "a row without a time comes first");

    let before = catalog(&store).await;
    let gaps = store.event_time_gaps(None).await.unwrap();
    assert_eq!(gaps.len(), 1);
    let gap = &gaps[0];
    assert_eq!(gap.records, 6);
    assert_eq!(
        (
            gap.missing_first,
            gap.first_fillable,
            gap.first_creation_not_held,
            gap.first_unfillable
        ),
        (4, 2, 1, 1)
    );
    assert_eq!(
        (
            gap.missing_last,
            gap.last_fillable,
            gap.last_held_by_snapshot,
            gap.last_unfillable
        ),
        (4, 2, 1, 1)
    );
    assert_eq!(store.event_time_gaps(Some(TENANT)).await.unwrap(), gaps);
    assert_eq!(catalog(&store).await, before, "the count writes nothing");
    let cursors: i64 = crate::dbm::postgres_query_scalar!(
        &schema,
        "SELECT COUNT(*) FROM {schema}event_time_backfill"
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cursors, 0);

    let filled = store.fill_event_times().await.unwrap();
    assert_eq!(
        (
            filled.records,
            filled.first_filled,
            filled.last_filled,
            filled.rows_written
        ),
        (6, 2, 2, 3),
        "october both at once, lost its last, snapshotted its first"
    );
    assert_eq!(
        times(&store, "october").await,
        (
            Some("2026-10-05T07:00:00.500000000Z".into()),
            Some("2026-10-06T08:00:00Z".into())
        )
    );
    assert_eq!(
        times(&store, "set").await,
        (
            Some("2026-09-01T00:00:00.000000000Z".into()),
            Some("2026-09-02T00:00:00Z".into())
        )
    );
    assert_eq!(times(&store, "empty").await, (None, None));
    assert_eq!(
        times(&store, "lost").await,
        (None, Some("2026-10-06T08:00:00Z".into()))
    );
    assert_eq!(
        times(&store, "snapshotted").await,
        (Some("2026-10-05T07:00:00.500000000Z".into()), None)
    );
    let oldest = by_last_event(&store, false).await;
    assert_eq!(&oldest[..3], ["set", "lost", "october"]);
    let newest = by_last_event(&store, true).await;
    assert_eq!(newest[2], "yesterday");

    let after = catalog(&store).await;
    reset_cursor(&store).await;
    assert_eq!(store.fill_event_times().await.unwrap().rows_written, 0);
    assert_eq!(catalog(&store).await, after, "a second run writes nothing");

    store
        .upsert_query_projection_with_state(
            TENANT,
            TYPE,
            "october",
            "Done",
            &json!({}),
            &json!({
                "entity_type": TYPE, "entity_id": "october", "status": "Done",
                "sequence_nr": 3, "events": [],
                "first_event_at": null, "last_event_at": "2026-11-19T10:00:00Z",
            }),
            3,
        )
        .await
        .unwrap();
    assert_eq!(
        times(&store, "october").await,
        (
            Some("2026-10-05T07:00:00.500000000Z".into()),
            Some("2026-11-19T10:00:00Z".into())
        )
    );
    drop_schema(&pool, &schema).await;
}

#[tokio::test]
async fn an_interrupted_postgres_fill_finishes_from_its_cursor() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    let pool = PgPool::connect(&url).await.unwrap();
    let (store, schema) = fresh_store(&pool).await;
    for n in 0..300 {
        seed(
            &store,
            &format!("ask-{n:04}"),
            &[(1, "2026-10-05T09:00:00Z"), (2, "2026-10-06T08:00:00Z")],
            (None, None),
            None,
        )
        .await;
    }
    assert_eq!(store.backfill_event_time_batch().await.unwrap(), None);
    let filled: i64 = crate::dbm::postgres_query_scalar!(
        &schema,
        "SELECT COUNT(*) FROM {schema}entity_catalog WHERE state ->> 'last_event_at' IS NOT NULL"
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(filled, 128);
    // A fresh store resumes from that cursor.
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    let totals = store.fill_event_times().await.unwrap();
    assert_eq!(
        (
            totals.records,
            totals.first_filled,
            totals.last_filled,
            totals.rows_written
        ),
        (300, 300, 300, 300)
    );
    assert_eq!(store.fill_event_times().await.unwrap(), totals);
    drop_schema(&pool, &schema).await;
}
