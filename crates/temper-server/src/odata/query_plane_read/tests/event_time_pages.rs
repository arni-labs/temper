//! Lists ordered and bounded by an event time, read past the scan budget
//! (ADR-0187). The budget is small (`max_entities` 20, so 210 candidates a
//! read) and the store holds more rows than that: every read here used to
//! answer 413, and each must now give the rows the in-memory sort gives.

use super::*;
use temper_odata::query::types::parse_query_options;
use temper_runtime::persistence::{MISSING_EVENT_TIME_KEY, event_time_key};

const ROWS: usize = 240;
const STATUSES: [&str; 3] = ["Open", "Done", "Held"];
const BUDGET: QueryPlaneReadBudget = QueryPlaneReadBudget {
    default_page_size: 20,
    max_entities: 20,
};

/// `(entity_id, status, last key, first key)` of every seeded row.
type Row = (String, String, String, String);

/// Row `index`'s event time at `minute`: ties, several fraction widths, and
/// some rows without one.
fn event_time(index: usize, minute: usize) -> Option<String> {
    if index.is_multiple_of(17) {
        return None;
    }
    let second = (index * 7) % 53;
    let fraction = match index % 4 {
        0 => String::new(),
        1 => format!(".{:03}", (index * 3) % 1000),
        2 => format!(".{:06}", index * 11),
        _ => format!(".{:09}", index * 13),
    };
    Some(format!("2026-11-19T10:{minute:02}:{second:02}{fraction}Z"))
}

fn key(time: &Option<String>) -> String {
    time.as_deref()
        .and_then(event_time_key)
        .unwrap_or_else(|| MISSING_EVENT_TIME_KEY.to_string())
}

async fn seeded_state(system_name: &str) -> (ServerState, Vec<Row>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("event-time.db").display()),
        None,
    )
    .await
    .expect("turso store");
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();
    let mut rows = Vec::new();
    for index in 0..ROWS {
        let id = format!("ord-{index:03}");
        let status = STATUSES[index % STATUSES.len()];
        let last = event_time(index, (index * 13) % 60);
        let first = event_time(index + 1, index % 60);
        let mut state_json = serde_json::json!({"status": status});
        if let Some(last) = &last {
            state_json["last_event_at"] = serde_json::json!(last);
        }
        if let Some(first) = &first {
            state_json["first_event_at"] = serde_json::json!(first);
        }
        store
            .upsert_query_projection_with_state(
                tenant.as_str(),
                "Order",
                &id,
                status,
                &serde_json::json!({"Id": id}),
                &state_json,
                1,
            )
            .await
            .expect("project row");
        rows.push((id, status.to_string(), key(&last), key(&first)));
    }
    (state, rows, dir)
}

/// The kept rows' ids in the in-memory order: by the key, a missing time
/// last ascending and first descending, then by id ascending.
fn expected(
    rows: &[Row],
    last: bool,
    descending: bool,
    keep: impl Fn(&Row) -> bool,
) -> Vec<String> {
    let mut kept: Vec<(&String, &String)> = rows
        .iter()
        .filter(|row| keep(row))
        .map(|row| (if last { &row.2 } else { &row.3 }, &row.0))
        .collect();
    kept.sort_by(|a, b| {
        let by_key = if descending {
            b.0.cmp(a.0)
        } else {
            a.0.cmp(b.0)
        };
        by_key.then_with(|| a.1.cmp(b.1))
    });
    kept.into_iter().map(|(_, id)| id.clone()).collect()
}

async fn read(
    state: &ServerState,
    security_ctx: &SecurityContext,
    query: &str,
) -> QueryPlaneReadResult {
    let query_options = parse_query_options(query).expect("query parses");
    let tenant = TenantId::default();
    match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state,
        tenant: &tenant,
        security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: BUDGET,
    })
    .await
    {
        Ok(result) => result,
        Err(QueryPlaneReadError::QueryTooLarge { telemetry }) => {
            panic!(
                "{query}: refused as too large after {} candidates",
                telemetry.candidate_count
            )
        }
        Err(_) => panic!("{query}: read failed"),
    }
}

/// Follow the continuation from the first page to the last.
async fn walk(state: &ServerState, security_ctx: &SecurityContext, query: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page_query = match &token {
            Some(token) => format!("{query}&$skiptoken={token}"),
            None => query.to_string(),
        };
        let result = read(state, security_ctx, &page_query).await;
        assert!(result.entities.len() <= BUDGET.max_entities);
        ids.extend(result.entities.iter().map(listed_entity_id));
        match result.next_skiptoken {
            Some(next) => token = Some(next),
            None => break,
        }
        assert!(
            ids.len() <= ROWS,
            "{query}: the continuation did not move on"
        );
    }
    ids
}

#[tokio::test]
async fn an_event_time_order_walks_every_row_past_the_scan_budget() {
    let (state, rows, _dir) = seeded_state("event-time-walk").await;
    let system = SecurityContext::system();
    for (query, last, descending) in [
        ("$orderby=last_event_at desc&$top=20", true, true),
        ("$orderby=last_event_at asc&$top=20", true, false),
        ("$orderby=first_event_at asc&$top=20", false, false),
        ("$orderby=first_event_at desc&$top=20", false, true),
    ] {
        let ids = walk(&state, &system, query).await;
        assert_eq!(ids, expected(&rows, last, descending, |_| true), "{query}");
    }
}

#[tokio::test]
async fn a_status_filtered_event_time_walk_and_its_count_pass_the_budget() {
    let (state, rows, _dir) = seeded_state("event-time-status-walk").await;
    let system = SecurityContext::system();
    let open_or_held = |row: &Row| row.1 == "Open" || row.1 == "Held";
    let filter = "$filter=(status eq 'Open' or status eq 'Held')";
    let want = expected(&rows, true, true, open_or_held);
    assert!(
        want.len() > 210,
        "the filtered set is larger than the budget"
    );

    let ids = walk(
        &state,
        &system,
        &format!("{filter}&$orderby=last_event_at desc&$top=7"),
    )
    .await;
    assert_eq!(ids, want);

    let first_page = read(
        &state,
        &system,
        &format!("{filter}&$orderby=last_event_at desc&$top=20&$count=true"),
    )
    .await;
    assert_eq!(first_page.count, Some(want.len()));
    let first_ids: Vec<String> = first_page.entities.iter().map(listed_entity_id).collect();
    assert_eq!(first_ids, want[..20].to_vec());

    let count_only = read(&state, &system, &format!("{filter}&$top=0&$count=true")).await;
    assert_eq!(count_only.count, Some(want.len()));
    assert!(count_only.entities.is_empty());

    let everything = read(&state, &system, "$top=0&$count=true").await;
    assert_eq!(everything.count, Some(ROWS));
}

#[tokio::test]
async fn a_page_of_max_entities_from_a_larger_set_is_a_full_page() {
    let (state, rows, _dir) = seeded_state("event-time-full-page").await;
    let system = SecurityContext::system();
    for (query, want) in [
        ("$top=20".to_string(), expected(&rows, true, true, |_| true)),
        (
            "$orderby=last_event_at desc&$top=20".to_string(),
            expected(&rows, true, true, |_| true),
        ),
        (
            "$filter=status eq 'Open'&$orderby=first_event_at asc&$top=20".to_string(),
            expected(&rows, false, false, |row| row.1 == "Open"),
        ),
    ] {
        let result = read(&state, &system, &query).await;
        assert_eq!(result.entities.len(), 20, "{query}");
        assert!(
            result.next_skiptoken.is_some(),
            "{query}: a full page continues"
        );
        if query.contains("orderby") {
            let ids: Vec<String> = result.entities.iter().map(listed_entity_id).collect();
            assert_eq!(ids, want[..20].to_vec(), "{query}");
        }
    }
}

#[tokio::test]
async fn an_event_time_bound_selects_by_instant_in_sql() {
    let (state, rows, _dir) = seeded_state("event-time-bounds").await;
    let system = SecurityContext::system();
    // `+01:00` is the same instant as 10:20 UTC; the bound compares instants.
    let since = event_time_key("2026-11-19T10:20:00Z").expect("key");
    let until = event_time_key("2026-11-19T10:45:30.5Z").expect("key");
    let query = "$filter=last_event_at ge '2026-11-19T11:20:00%2B01:00' and \
                 last_event_at lt '2026-11-19T10:45:30.5Z' and status ne 'Done'\
                 &$orderby=last_event_at asc&$top=20";
    let want = expected(&rows, true, false, |row| {
        row.2 >= since && row.2 < until && row.1 != "Done"
    });
    assert!(!want.is_empty());
    assert_eq!(walk(&state, &system, query).await, want);

    let counted = read(
        &state,
        &system,
        "$filter=last_event_at gt '2026-11-19T10:20:00Z'&$top=0&$count=true",
    )
    .await;
    let after = rows
        .iter()
        .filter(|row| row.2 > since && row.2 != MISSING_EVENT_TIME_KEY)
        .count();
    assert_eq!(counted.count, Some(after));
}

#[tokio::test]
async fn a_per_row_policy_walk_shows_only_allowed_rows_in_order() {
    let (state, rows, _dir) = seeded_state("event-time-per-row").await;
    let tenant = TenantId::default();
    let hidden: Vec<String> = (0..ROWS)
        .step_by(5)
        .map(|index| format!("ord-{index:03}"))
        .collect();
    let forbids: String = hidden
        .iter()
        .map(|id| {
            format!("forbid(principal, action == Action::\"read\", resource == Order::\"{id}\");\n")
        })
        .collect();
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            &format!(
                "permit(principal, action == Action::\"list\", resource is Order);\n\
                 permit(principal, action == Action::\"read\", resource is Order);\n{forbids}"
            ),
        )
        .expect("install row forbids");
    let reader = SecurityContext {
        principal: temper_authz::Principal {
            id: "reader".to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "event-time-per-row".to_string(),
        cause: None,
        credential: None,
    };
    let ids = walk(&state, &reader, "$orderby=last_event_at desc&$top=20").await;
    let want = expected(&rows, true, true, |row| !hidden.contains(&row.0));
    assert_eq!(ids, want);
}
