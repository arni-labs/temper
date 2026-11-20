//! ADR-0187 "Backfill": a record made before the kernel kept event times and
//! not moved since sorts by its journal's times once the fill has run, the
//! count writes nothing, and the fill never replaces a time, writes each
//! record once, resumes after a stop and leaves what it cannot know.

use std::time::Duration;

use serde_json::json;
use temper_runtime::persistence::{EventTimeGaps, QueryFieldIndexOrder};

use super::super::WritePriority;
use super::super::unchanged_projections::projection_hash;
use super::*;

const TYPE: &str = "Ask";

/// A catalog row as a record made in October and not moved since has it: no
/// `first_event_at` and no `last_event_at`, or the given ones; and its
/// journal, `(sequence, time)` per event, with a snapshot at `snapshot`.
struct Record<'a> {
    id: &'a str,
    events: &'a [(i64, &'a str)],
    first: Option<&'a str>,
    last: Option<&'a str>,
    snapshot: Option<i64>,
}

impl<'a> Record<'a> {
    fn october(id: &'a str, events: &'a [(i64, &'a str)]) -> Self {
        Self {
            id,
            events,
            first: None,
            last: None,
            snapshot: None,
        }
    }
}

async fn seed(store: &TursoEventStore, tenant: &str, record: &Record<'_>) {
    let conn = store
        .write_connection("test", WritePriority::High)
        .await
        .unwrap();
    let sequence = record.events.iter().map(|event| event.0).max().unwrap_or(1);
    let mut state = json!({
        "entity_type": TYPE, "entity_id": record.id, "status": "Open",
        "sequence_nr": sequence, "events": [],
    });
    if let Some(last) = record.last {
        state["last_event_at"] = json!(last);
    }
    conn.execute(
        "INSERT INTO entity_catalog (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, first_event_at) \
         VALUES (?1, ?2, ?3, 'Open', '{}', ?4, '2026-10-06', ?5, ?6)",
        params![
            tenant,
            TYPE,
            record.id,
            state.to_string(),
            sequence,
            record.first.map(str::to_string)
        ],
    )
    .await
    .unwrap();
    for (sequence, at) in record.events {
        let stamp = json!({ "timestamp": at }).to_string();
        conn.execute(
            "INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) \
             VALUES (?1, ?2, ?3, ?4, 'Moved', ?5, ?5)",
            params![tenant, TYPE, record.id, *sequence, stamp],
        )
        .await
        .unwrap();
    }
    if let Some(snapshot) = record.snapshot {
        conn.execute(
            "INSERT INTO snapshots (tenant, entity_type, entity_id, sequence_nr, snapshot) VALUES (?1, ?2, ?3, ?4, x'00')",
            params![tenant, TYPE, record.id, snapshot],
        )
        .await
        .unwrap();
    }
}

/// A record made yesterday, as a move writes it now.
async fn moved_yesterday(store: &TursoEventStore, tenant: &str) {
    store
        .upsert_query_projection_with_state(
            tenant,
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
}

/// The ids of `tenant`'s records ordered by `last_event_at`.
async fn by_last_event(store: &TursoEventStore, tenant: &str, descending: bool) -> Vec<String> {
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
        .query_field_index_page(tenant, TYPE, &page)
        .await
        .unwrap()
        .0
}

/// Every catalog row, whole, in key order: what "writes nothing" compares.
async fn catalog(store: &TursoEventStore) -> Vec<String> {
    let conn = store.connection().await.unwrap();
    let mut rows = conn
        .query(
            "SELECT tenant || '|' || entity_type || '|' || entity_id || '|' || status || '|' || fields || '|' \
                 || COALESCE(state, '') || '|' || COALESCE(first_event_at, '') || '|' || projection_hash || '|' \
                 || updated_at || '|' || sequence_nr \
             FROM entity_catalog ORDER BY tenant, entity_type, entity_id",
            (),
        )
        .await
        .unwrap();
    let mut found = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        found.push(row.get::<String>(0).unwrap());
    }
    found
}

/// One row's `first_event_at`, state, status, fields and projection hash.
async fn row(
    store: &TursoEventStore,
    tenant: &str,
    id: &str,
) -> (Option<String>, serde_json::Value, String, String, String) {
    let conn = store.connection().await.unwrap();
    let mut rows = conn
        .query(
            "SELECT first_event_at, state, status, fields, projection_hash \
             FROM entity_catalog INDEXED BY sqlite_autoindex_entity_catalog_1 \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, TYPE, id],
        )
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    (
        row.get(0).unwrap(),
        serde_json::from_str(&row.get::<String>(1).unwrap()).unwrap(),
        row.get(2).unwrap(),
        row.get(3).unwrap(),
        row.get(4).unwrap(),
    )
}

async fn cursor_rows(store: &TursoEventStore) -> i64 {
    let conn = store.connection().await.unwrap();
    let mut rows = conn
        .query("SELECT COUNT(*) FROM event_time_backfill", ())
        .await
        .unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

/// Run the fill again from the start, as a rerun after a reset cursor would.
async fn refill(store: &TursoEventStore) -> temper_runtime::persistence::EventTimeFill {
    let conn = store
        .write_connection("test", WritePriority::High)
        .await
        .unwrap();
    conn.execute("DELETE FROM event_time_backfill", ())
        .await
        .unwrap();
    drop(conn);
    store.fill_event_times().await.unwrap()
}

fn only(gaps: &[EventTimeGaps]) -> &EventTimeGaps {
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    &gaps[0]
}

#[tokio::test]
async fn an_october_record_sorts_by_its_journal_times_once_filled_and_keeps_them_after_a_move() {
    let store = make_store("event-time-october").await;
    let tenant = "app";
    moved_yesterday(&store, tenant).await;
    seed(
        &store,
        tenant,
        &Record::october(
            "october",
            &[
                (1, "2026-10-05T09:00:00.5+02:00"),
                (2, "2026-10-06T08:00:00Z"),
            ],
        ),
    )
    .await;

    // Today: the record without a time sorts first newest-first and last
    // oldest-first, whatever its journal says.
    assert_eq!(
        by_last_event(&store, tenant, true).await,
        ["october", "yesterday"]
    );
    assert_eq!(
        by_last_event(&store, tenant, false).await,
        ["yesterday", "october"]
    );

    // The count reports it and writes nothing, not even a cursor.
    let before = catalog(&store).await;
    let gaps = store.event_time_gaps(None).await.unwrap();
    let gap = only(&gaps);
    assert_eq!(
        (gap.records, gap.missing_first, gap.first_fillable),
        (2, 1, 1)
    );
    assert_eq!((gap.missing_last, gap.last_fillable), (1, 1));
    assert_eq!(store.event_time_gaps(Some(tenant)).await.unwrap(), gaps);
    assert!(
        store
            .event_time_gaps(Some("other"))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(catalog(&store).await, before);
    assert_eq!(cursor_rows(&store).await, 0);

    let filled = store.fill_event_times().await.unwrap();
    assert_eq!(
        (
            filled.records,
            filled.first_filled,
            filled.last_filled,
            filled.rows_written
        ),
        (2, 1, 1, 1),
        "one row written for the one record that changed"
    );
    assert_eq!(
        by_last_event(&store, tenant, true).await,
        ["yesterday", "october"]
    );
    assert_eq!(
        by_last_event(&store, tenant, false).await,
        ["october", "yesterday"]
    );
    let (first, state, status, fields, hash) = row(&store, tenant, "october").await;
    assert_eq!(first.as_deref(), Some("2026-10-05T07:00:00.500000000Z"));
    assert_eq!(state["last_event_at"], "2026-10-06T08:00:00Z");
    assert_eq!(hash, projection_hash(&status, &fields, &state));
    let gap = store.event_time_gaps(None).await.unwrap();
    assert_eq!((only(&gap).missing_first, only(&gap).missing_last), (0, 0));

    // A second run writes nothing.
    let after = catalog(&store).await;
    assert_eq!(refill(&store).await.rows_written, 0);
    assert_eq!(catalog(&store).await, after);
    assert!(
        after
            .iter()
            .find(|row| row.contains("|october|"))
            .is_some_and(|row| row.contains("|2026-10-06|")),
        "the fill is not a move: {after:?}"
    );

    // The next move, made by an actor that recovered no creation time, keeps
    // the filled one and moves the last time on.
    store
        .upsert_query_projection_with_state(
            tenant,
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
    let (first, state, ..) = row(&store, tenant, "october").await;
    assert_eq!(first.as_deref(), Some("2026-10-05T07:00:00.500000000Z"));
    assert_eq!(state["last_event_at"], "2026-11-19T10:00:00Z");
    assert_eq!(
        by_last_event(&store, tenant, true).await,
        ["october", "yesterday"]
    );
}

#[tokio::test]
async fn the_fill_leaves_set_times_lost_creations_snapshotted_records_and_empty_journals() {
    let store = make_store("event-time-left").await;
    let tenant = "app";
    let journal = [(1, "2026-10-05T09:00:00Z"), (2, "2026-10-06T08:00:00Z")];
    // Both times set, and different from the journal's.
    seed(
        &store,
        tenant,
        &Record {
            first: Some("2026-09-01T00:00:00.000000000Z"),
            last: Some("2026-09-02T00:00:00Z"),
            ..Record::october("set", &journal)
        },
    )
    .await;
    // No journal at all.
    seed(&store, tenant, &Record::october("empty", &[])).await;
    // Its creation event is no longer held.
    seed(
        &store,
        tenant,
        &Record::october("lost", &[(2, "2026-10-06T08:00:00Z")]),
    )
    .await;
    // Its newest event is in a snapshot that predates the time.
    seed(
        &store,
        tenant,
        &Record {
            snapshot: Some(2),
            ..Record::october("snapshotted", &journal)
        },
    )
    .await;
    let set_before = catalog(&store).await;

    let gaps = store.event_time_gaps(None).await.unwrap();
    let gap = only(&gaps);
    assert_eq!(gap.records, 4);
    assert_eq!(
        (
            gap.missing_first,
            gap.first_fillable,
            gap.first_creation_not_held,
            gap.first_unfillable
        ),
        (3, 1, 1, 1)
    );
    assert_eq!(
        (
            gap.missing_last,
            gap.last_fillable,
            gap.last_held_by_snapshot,
            gap.last_unfillable
        ),
        (3, 1, 1, 1)
    );

    let filled = store.fill_event_times().await.unwrap();
    assert_eq!(
        (filled.first_filled, filled.last_filled, filled.rows_written),
        (1, 1, 2)
    );
    let after = catalog(&store).await;
    for id in ["|set|", "|empty|"] {
        let find = |rows: &[String]| rows.iter().find(|row| row.contains(id)).cloned();
        assert_eq!(find(&after), find(&set_before), "{id} is left as it was");
    }
    let (first, state, ..) = row(&store, tenant, "lost").await;
    assert_eq!(first, None, "a later event is not when the record was made");
    assert_eq!(state["last_event_at"], "2026-10-06T08:00:00Z");
    let (first, state, ..) = row(&store, tenant, "snapshotted").await;
    assert_eq!(first.as_deref(), Some("2026-10-05T09:00:00.000000000Z"));
    assert!(state.get("last_event_at").is_none(), "{state}");
    let gap = store.event_time_gaps(None).await.unwrap();
    let gap = only(&gap);
    assert_eq!(
        (
            gap.first_creation_not_held,
            gap.first_unfillable,
            gap.first_fillable
        ),
        (1, 1, 0)
    );
    assert_eq!(
        (
            gap.last_held_by_snapshot,
            gap.last_unfillable,
            gap.last_fillable
        ),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn an_interrupted_fill_finishes_from_its_cursor() {
    let url = sqlite_test_url("event-time-resume");
    let store = TursoEventStore::new(&url, None).await.unwrap();
    for n in 0..300 {
        let id = format!("ask-{n:04}");
        seed(
            &store,
            "app",
            &Record::october(
                &id,
                &[(1, "2026-10-05T09:00:00Z"), (2, "2026-10-06T08:00:00Z")],
            ),
        )
        .await;
    }
    // Rows this small fit many in one hold: the batch stops at its row count.
    assert_eq!(
        store
            .backfill_event_time_batch(Duration::MAX)
            .await
            .unwrap(),
        None
    );
    let filled = {
        let conn = store.connection().await.unwrap();
        let mut rows = conn
            .query(
                "SELECT COUNT(*) FROM entity_catalog WHERE json_extract(state, '$.last_event_at') IS NOT NULL",
                (),
            )
            .await
            .unwrap();
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
    };
    assert_eq!(
        filled, 128,
        "the stopped batch's records are whole, the rest untouched"
    );
    drop(store);

    let store = TursoEventStore::new(&url, None).await.unwrap();
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
    let gap = store.event_time_gaps(None).await.unwrap();
    assert_eq!((only(&gap).missing_first, only(&gap).missing_last), (0, 0));
    // Finished: running it again neither visits nor writes.
    assert_eq!(store.fill_event_times().await.unwrap(), totals);
}
