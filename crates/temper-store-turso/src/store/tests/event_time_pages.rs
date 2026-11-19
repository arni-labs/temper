//! Pages ordered and bounded by an event time, resumed by a keyset in SQL
//! (ADR-0187): every row once, in the order the in-memory sort gives, and
//! read through the event-time index without a sort.

use temper_runtime::persistence::{
    EventTimeBound, EventTimeColumn, KeyComparison, MISSING_EVENT_TIME_KEY, QueryFieldIndexOrder,
    QueryFieldIndexOrderDirection, QueryFieldIndexPageQuery, event_time_key,
};

use super::make_store;
use crate::driver::{Value, params_from_iter};

const ENTITY_TYPE: &str = "Card";
const ROWS: usize = 60;
const STATUSES: [&str; 3] = ["Open", "Done", "Held"];

/// Row `index`'s event time: ties, every fraction width the kernel writes,
/// and some rows with none (no key, or a JSON null).
fn event_time(index: usize, minute: usize) -> Option<String> {
    if matches!(index % 10, 0 | 5) {
        return None;
    }
    let second = (index * 7) % 23;
    let fraction = match index % 4 {
        0 => String::new(),
        1 => format!(".{:03}", index * 3),
        2 => format!(".{:06}", index * 11),
        _ => format!(".{:09}", index * 13),
    };
    Some(format!("2026-11-19T10:{minute:02}:{second:02}{fraction}Z"))
}

struct Seeded {
    tenant: String,
    /// `(entity_id, status, last key, first key)` of every row.
    rows: Vec<(String, String, String, String)>,
}

async fn seed(store: &crate::TursoEventStore) -> Seeded {
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let mut rows = Vec::new();
    for index in 0..ROWS {
        let entity_id = format!("r-{index:02}");
        let status = STATUSES[index % STATUSES.len()];
        let last = event_time(index, 30);
        let first = event_time((index * 3) % ROWS, 10);
        let mut state = serde_json::json!({
            "entity_type": ENTITY_TYPE,
            "entity_id": entity_id,
            "status": status,
            "fields": {},
            "sequence_nr": 1,
            "events": [],
        });
        if let Some(last) = &last {
            state["last_event_at"] = serde_json::json!(last);
        } else if index % 10 == 5 {
            state["last_event_at"] = serde_json::Value::Null;
        }
        if let Some(first) = &first {
            state["first_event_at"] = serde_json::json!(first);
        }
        store
            .upsert_query_projection_with_state(
                &tenant,
                ENTITY_TYPE,
                &entity_id,
                status,
                &serde_json::json!({}),
                &state,
                1,
            )
            .await
            .expect("project row");
        let key = |time: &Option<String>| {
            time.as_deref()
                .and_then(event_time_key)
                .unwrap_or_else(|| MISSING_EVENT_TIME_KEY.to_string())
        };
        rows.push((entity_id, status.to_string(), key(&last), key(&first)));
    }
    Seeded { tenant, rows }
}

fn ordered_page(column: EventTimeColumn, descending: bool) -> QueryFieldIndexPageQuery {
    QueryFieldIndexPageQuery {
        where_clause: "TRUE".to_string(),
        order_by: vec![QueryFieldIndexOrder {
            field_name: column.property().to_string(),
            direction: if descending {
                QueryFieldIndexOrderDirection::Desc
            } else {
                QueryFieldIndexOrderDirection::Asc
            },
        }],
        top: 7,
        ..QueryFieldIndexPageQuery::default()
    }
}

/// The rows `keep` selects, sorted as the in-memory sort orders them: by the
/// key in the page's direction, a missing time last ascending and first
/// descending, then by id ascending.
fn expected(
    seeded: &Seeded,
    column: EventTimeColumn,
    descending: bool,
    keep: impl Fn(&(String, String, String, String)) -> bool,
) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = seeded
        .rows
        .iter()
        .filter(|row| keep(row))
        .map(|row| {
            let key = match column {
                EventTimeColumn::Last => row.2.clone(),
                EventTimeColumn::First => row.3.clone(),
            };
            (key, row.0.clone())
        })
        .collect();
    rows.sort_by(|a, b| {
        let by_key = if descending {
            b.0.cmp(&a.0)
        } else {
            a.0.cmp(&b.0)
        };
        by_key.then_with(|| a.1.cmp(&b.1))
    });
    rows
}

/// Follow the keyset from the first page to the last.
async fn walk(
    store: &crate::TursoEventStore,
    tenant: &str,
    mut page: QueryFieldIndexPageQuery,
    keys: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut ids = Vec::new();
    loop {
        let (page_ids, _) = store
            .query_field_index_page(tenant, ENTITY_TYPE, &page)
            .await
            .expect("page");
        assert!(page_ids.len() <= page.top);
        ids.extend(page_ids.iter().cloned());
        let Some(last) = page_ids.last() else { break };
        if page_ids.len() < page.top {
            break;
        }
        page.after = Some(vec![keys[last].clone(), last.clone()]);
        assert!(ids.len() <= ROWS, "the keyset did not move on");
    }
    ids
}

#[tokio::test]
async fn a_keyset_walk_gives_every_row_once_in_event_time_order() {
    let store = make_store("event-time-keyset-walk").await;
    let seeded = seed(&store).await;
    for (column, descending) in [
        (EventTimeColumn::Last, true),
        (EventTimeColumn::Last, false),
        (EventTimeColumn::First, true),
        (EventTimeColumn::First, false),
    ] {
        let expected_rows = expected(&seeded, column, descending, |_| true);
        let keys = expected_rows
            .iter()
            .map(|(key, id)| (id.clone(), key.clone()))
            .collect();
        let ids = walk(
            &store,
            &seeded.tenant,
            ordered_page(column, descending),
            &keys,
        )
        .await;
        let want: Vec<String> = expected_rows.into_iter().map(|(_, id)| id).collect();
        assert_eq!(ids, want, "{column:?} descending={descending}");
    }
}

#[tokio::test]
async fn a_status_filtered_walk_and_count_stay_in_their_states() {
    let store = make_store("event-time-status-walk").await;
    let seeded = seed(&store).await;
    for (where_clause, params, states) in [
        ("status = ?3", vec!["Open"], vec!["Open"]),
        (
            "(status = ?3 OR status = ?4)",
            vec!["Open", "Held"],
            vec!["Open", "Held"],
        ),
    ] {
        let keep = |row: &(String, String, String, String)| states.contains(&row.1.as_str());
        let expected_rows = expected(&seeded, EventTimeColumn::Last, true, keep);
        let keys = expected_rows
            .iter()
            .map(|(key, id)| (id.clone(), key.clone()))
            .collect();
        let page = QueryFieldIndexPageQuery {
            where_clause: where_clause.to_string(),
            params: params.iter().map(|value| value.to_string()).collect(),
            ..ordered_page(EventTimeColumn::Last, true)
        };
        let ids = walk(&store, &seeded.tenant, page.clone(), &keys).await;
        let want: Vec<String> = expected_rows.iter().map(|(_, id)| id.clone()).collect();
        assert_eq!(ids, want, "{where_clause}");

        let count_only = QueryFieldIndexPageQuery {
            top: 0,
            include_count: true,
            ..page.clone()
        };
        let (none, count) = store
            .query_field_index_page(&seeded.tenant, ENTITY_TYPE, &count_only)
            .await
            .expect("count");
        assert!(none.is_empty());
        assert_eq!(count, Some(want.len()), "{where_clause}");

        let counted = QueryFieldIndexPageQuery {
            include_count: true,
            ..page
        };
        let (first, count) = store
            .query_field_index_page(&seeded.tenant, ENTITY_TYPE, &counted)
            .await
            .expect("page and count");
        assert_eq!(first, want[..7].to_vec());
        assert_eq!(count, Some(want.len()));
    }
}

#[tokio::test]
async fn an_event_time_bound_selects_by_time_and_never_a_row_without_one() {
    let store = make_store("event-time-bounds").await;
    let seeded = seed(&store).await;
    let since = event_time_key("2026-11-19T10:30:11Z").expect("key");
    let until = event_time_key("2026-11-19T10:30:17.5Z").expect("key");
    let page = QueryFieldIndexPageQuery {
        event_time_bounds: vec![
            EventTimeBound {
                column: EventTimeColumn::Last,
                comparison: KeyComparison::Ge,
                key: since.clone(),
            },
            EventTimeBound {
                column: EventTimeColumn::Last,
                comparison: KeyComparison::Lt,
                key: until.clone(),
            },
        ],
        top: ROWS,
        include_count: true,
        ..ordered_page(EventTimeColumn::Last, false)
    };
    let (ids, count) = store
        .query_field_index_page(&seeded.tenant, ENTITY_TYPE, &page)
        .await
        .expect("bounded page");
    let want: Vec<String> = expected(&seeded, EventTimeColumn::Last, false, |row| {
        row.2 >= since && row.2 < until
    })
    .into_iter()
    .map(|(_, id)| id)
    .collect();
    assert!(!want.is_empty());
    assert_eq!(ids, want);
    assert_eq!(count, Some(want.len()));

    let open_ended = QueryFieldIndexPageQuery {
        event_time_bounds: vec![EventTimeBound {
            column: EventTimeColumn::Last,
            comparison: KeyComparison::Gt,
            key: since.clone(),
        }],
        ..page
    };
    let (ids, _) = store
        .query_field_index_page(&seeded.tenant, ENTITY_TYPE, &open_ended)
        .await
        .expect("open-ended page");
    let missing: Vec<&String> = seeded
        .rows
        .iter()
        .filter(|row| row.2 == MISSING_EVENT_TIME_KEY)
        .map(|row| &row.0)
        .collect();
    assert!(!missing.is_empty());
    assert!(ids.iter().all(|id| !missing.contains(&id)));
}

/// The plan lines of the statement a page runs.
async fn plan(
    store: &crate::TursoEventStore,
    tenant: &str,
    page: &QueryFieldIndexPageQuery,
) -> Vec<String> {
    let (where_clause, params) = super::super::query_page::page_where(page).expect("where");
    let sql = super::super::query_page::turso_query_field_page_sql(
        page,
        &where_clause,
        params.len(),
        false,
    );
    let mut values = vec![Value::from(tenant.to_string()), Value::from(ENTITY_TYPE)];
    values.extend(params.into_iter().map(Value::from));
    values.push(Value::from(page.top as i64));
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            &format!("EXPLAIN QUERY PLAN {sql}"),
            params_from_iter(values),
        )
        .await
        .expect("query plan");
    let mut lines = Vec::new();
    while let Some(row) = rows.next().await.expect("plan row") {
        lines.push(row.get::<String>(3).expect("plan detail"));
    }
    lines
}

#[tokio::test]
async fn a_status_filtered_page_reads_the_event_time_index_in_order() {
    let store = make_store("event-time-plan").await;
    let seeded = seed(&store).await;
    let first_page = QueryFieldIndexPageQuery {
        where_clause: "status = ?3".to_string(),
        params: vec!["Open".to_string()],
        ..ordered_page(EventTimeColumn::Last, true)
    };
    let continuation = QueryFieldIndexPageQuery {
        after: Some(vec![
            event_time_key("2026-11-19T10:30:11Z").expect("key"),
            "r-03".to_string(),
        ]),
        ..first_page.clone()
    };
    for (name, page) in [("first page", &first_page), ("continuation", &continuation)] {
        let lines = plan(&store, &seeded.tenant, page).await;
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("SORTER") && !line.contains("TEMP B-TREE")),
            "{name}: the page is sorted, not read in index order: {lines:?}"
        );
        let search = lines
            .iter()
            .find(|line| line.contains("USING INDEX idx_entity_catalog_last_event_desc"))
            .unwrap_or_else(|| panic!("{name}: the event-time index is not read: {lines:?}"));
        if name == "continuation" {
            // The keyset is a range on the index's key: the read starts at
            // the cursor instead of passing over the rows before it.
            assert!(
                search.contains("COALESCE"),
                "continuation: the keyset is not a seek: {search}"
            );
        }
    }
}
