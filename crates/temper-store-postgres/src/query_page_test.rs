//! Pages ordered and bounded by an event time (ADR-0187). The live tests
//! are gated on DATABASE_URL (they skip locally without it and fail in CI)
//! and isolated by a unique tenant.

use super::*;
use crate::migration::run_migrations;
use sqlx::PgPool;
use temper_runtime::persistence::{
    EventTimeBound, KeyComparison, QueryFieldIndexOrder, QueryFieldIndexOrderDirection,
    event_time_key,
};

const ENTITY_TYPE: &str = "Card";
const ROWS: usize = 40;
const STATUSES: [&str; 3] = ["Open", "Done", "Held"];

/// The migration builds each index on the expression a page orders by; a
/// different text would leave the planner to sort.
#[test]
fn the_migration_indexes_the_expression_pages_order_by() {
    let migration = include_str!("../migrations/0029_event_time_order_indexes.sql");
    let normalize = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let migration = normalize(migration);
    for column in [EventTimeColumn::Last, EventTimeColumn::First] {
        let key = normalize(&event_time_key_sql(column));
        assert_eq!(
            migration.matches(&key).count(),
            2,
            "{column:?}: one index per direction on {key}"
        );
    }
}

/// Row `index`'s event time: ties, several fraction widths, and some rows
/// with none (no key, or a JSON null).
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

/// `(entity_id, status, last key, first key)` of every seeded row.
type Row = (String, String, String, String);

async fn seed(store: &PostgresEventStore, tenant: &str) -> Vec<Row> {
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
                tenant,
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
    rows
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

/// The kept rows as `(key, id)` in the order the in-memory sort gives.
fn expected(
    rows: &[Row],
    column: EventTimeColumn,
    descending: bool,
    keep: impl Fn(&Row) -> bool,
) -> Vec<(String, String)> {
    let mut kept: Vec<(String, String)> = rows
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
    kept.sort_by(|a, b| {
        let by_key = if descending {
            b.0.cmp(&a.0)
        } else {
            a.0.cmp(&b.0)
        };
        by_key.then_with(|| a.1.cmp(&b.1))
    });
    kept
}

/// Follow the keyset from the first page to the last.
async fn walk(
    store: &PostgresEventStore,
    tenant: &str,
    mut page: QueryFieldIndexPageQuery,
    expected_rows: &[(String, String)],
) -> Vec<String> {
    let keys: std::collections::BTreeMap<&str, &str> = expected_rows
        .iter()
        .map(|(key, id)| (id.as_str(), key.as_str()))
        .collect();
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
        page.after = Some(vec![
            keys[last.as_str()].to_string().into(),
            last.clone().into(),
        ]);
        assert!(ids.len() <= ROWS, "the keyset did not move on");
    }
    ids
}

async fn cleanup(pool: &PgPool, tenant: &str) {
    crate::dbm::postgres_query!("DELETE FROM entity_catalog WHERE tenant = $1")
        .bind(tenant)
        .execute(pool)
        .await
        .expect("cleanup");
}

#[test]
fn event_time_pages_walk_count_and_bound_inside_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrations");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("tenant-event-time-{}", uuid::Uuid::new_v4());
        let rows = seed(&store, &tenant).await;

        for (column, descending) in [
            (EventTimeColumn::Last, true),
            (EventTimeColumn::Last, false),
            (EventTimeColumn::First, true),
            (EventTimeColumn::First, false),
        ] {
            let want = expected(&rows, column, descending, |_| true);
            let ids = walk(&store, &tenant, ordered_page(column, descending), &want).await;
            let want: Vec<String> = want.into_iter().map(|(_, id)| id).collect();
            assert_eq!(ids, want, "{column:?} descending={descending}");
        }

        let open = |row: &Row| row.1 == "Open" || row.1 == "Held";
        let want = expected(&rows, EventTimeColumn::Last, true, open);
        let page = QueryFieldIndexPageQuery {
            where_clause: "(status = ?3 OR status = ?4)".to_string(),
            params: vec!["Open".to_string(), "Held".to_string()],
            ..ordered_page(EventTimeColumn::Last, true)
        };
        let ids = walk(&store, &tenant, page.clone(), &want).await;
        let want: Vec<String> = want.into_iter().map(|(_, id)| id).collect();
        assert_eq!(ids, want);
        let counted = QueryFieldIndexPageQuery {
            include_count: true,
            ..page.clone()
        };
        let (first, count) = store
            .query_field_index_page(&tenant, ENTITY_TYPE, &counted)
            .await
            .expect("page and count");
        assert_eq!(first, want[..7].to_vec());
        assert_eq!(count, Some(want.len()));
        let count_only = QueryFieldIndexPageQuery { top: 0, ..counted };
        let (none, count) = store
            .query_field_index_page(&tenant, ENTITY_TYPE, &count_only)
            .await
            .expect("count");
        assert!(none.is_empty());
        assert_eq!(count, Some(want.len()));

        let since = event_time_key("2026-11-19T10:30:11Z").expect("key");
        let bounded = QueryFieldIndexPageQuery {
            event_time_bounds: vec![EventTimeBound {
                column: EventTimeColumn::Last,
                comparison: KeyComparison::Gt,
                key: since.clone(),
            }],
            top: ROWS,
            ..ordered_page(EventTimeColumn::Last, false)
        };
        let (ids, _) = store
            .query_field_index_page(&tenant, ENTITY_TYPE, &bounded)
            .await
            .expect("bounded page");
        let want: Vec<String> = expected(&rows, EventTimeColumn::Last, false, |row| {
            row.2 > since && row.2 != MISSING_EVENT_TIME_KEY
        })
        .into_iter()
        .map(|(_, id)| id)
        .collect();
        assert!(!want.is_empty());
        assert_eq!(ids, want);

        cleanup(&pool, &tenant).await;
    });
}

/// With sorting priced out, the plan of a status-filtered continuation has
/// no Sort node only when an event-time index yields the page's order: the
/// expression `query_page.rs` orders by matches the one migration 0029
/// indexes.
#[test]
fn an_event_time_page_reads_its_index_in_order_inside_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrations");
        let page = QueryFieldIndexPageQuery {
            where_clause: "status = ?3".to_string(),
            params: vec!["Open".to_string()],
            after: Some(vec![
                event_time_key("2026-11-19T10:30:11Z").expect("key").into(),
                "r-03".into(),
            ]),
            ..ordered_page(EventTimeColumn::Last, true)
        };
        let (conditions, condition_params) = page
            .catalog_conditions(page.params.len() + 3, &PostgresCatalogSql)
            .expect("conditions");
        let mut params = page.params.clone();
        params.extend(condition_params);
        let filter = format!("({}) AND {conditions}", page.where_clause);
        let clause = postgres_placeholders(&filter, params.len() + 2);
        let sql = postgres_query_field_page_sql(
            "",
            &clause,
            &postgres_query_field_order_sql(&page),
            params.len(),
            false,
            false,
        );
        let mut tx = pool.begin().await.expect("transaction");
        for setting in [
            "SET LOCAL enable_sort = off",
            "SET LOCAL enable_seqscan = off",
        ] {
            sqlx::query(setting)
                .execute(&mut *tx)
                .await
                .expect("setting");
        }
        let explain = format!("EXPLAIN {sql}");
        let mut query = sqlx::query_scalar::<_, String>(&explain)
            .bind("tenant-plan")
            .bind(ENTITY_TYPE);
        for param in params {
            query = query.bind(param);
        }
        let lines = query
            .bind(page.top as i64)
            .fetch_all(&mut *tx)
            .await
            .expect("plan");
        tx.rollback().await.expect("rollback");
        assert!(
            lines.iter().all(|line| !line.contains("Sort")),
            "the page is sorted, not read in index order: {lines:#?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("idx_entity_catalog_last_event_desc")),
            "the event-time index is not read: {lines:#?}"
        );
    });
}
