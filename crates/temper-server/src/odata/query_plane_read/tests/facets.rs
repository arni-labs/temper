//! Facet reads (`$apply=groupby((p),aggregate($count as count))`) count the
//! records a list read would show by the value of one property, as the
//! evaluator reads it, and page through the groups by a keyset (ADR-0188).
//! The store groups in SQL only when every row is allowed and it holds the
//! filter exactly; every other read walks the records. Before, `$apply` was
//! ignored and the read answered a page of records.

use std::cmp::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_odata::query::types::parse_query_options;
use tower::ServiceExt;

use super::super::read_facets;
use super::large_set::{
    BUDGET, PostgresCards, ROWS, in_memory, install_reader, sim_cards, turso_cards, visible,
};
use super::*;

const PROPERTIES: [&str; 9] = [
    "Status", "kind", "title", "rank", "score", "pinned", "said_at", "up", "note",
];
const FILTERS: [&str; 5] = [
    "",
    "Status ne 'Held'",
    "rank gt 2",
    "contains(title,'p')",
    "kind eq 'steer'",
];
/// Keys SQL compares as the evaluator does and that hold one type here.
const SQL_KEYS: [&str; 5] = ["Status", "title", "rank", "score", "said_at"];
/// Filters SQL holds exactly (an equality goes by its reconciled read).
const SQL_FILTERS: [&str; 3] = ["", "Status ne 'Held'", "rank gt 2"];

fn orders(property: &str) -> [String; 4] {
    [
        String::new(),
        "count desc".to_string(),
        format!("count desc,{property} asc"),
        format!("{property} asc"),
    ]
}

fn facet_query(property: &str, filter: &str, order: &str) -> String {
    let mut query = format!("$apply=groupby(({property}),aggregate($count as count))");
    if !filter.is_empty() {
        query.push_str(&format!("&$filter={filter}"));
    }
    if !order.is_empty() {
        query.push_str(&format!("&$orderby={order}"));
    }
    query
}

async fn facet_page(
    state: &ServerState,
    reader: &SecurityContext,
    query: &str,
) -> Result<super::super::facets::FacetPage, QueryPlaneReadError> {
    let options = parse_query_options(query).expect("query parses");
    let group = options.apply.clone().expect("a grouping");
    let tenant = TenantId::default();
    read_facets(
        QueryPlaneReadRequest {
            state,
            tenant: &tenant,
            security_ctx: reader,
            entity_type: "Card",
            entity_set_name: "Cards",
            query_options: &options,
            budget: BUDGET,
        },
        &group,
    )
    .await
}

/// Every group, three a page, and whether the store grouped every page.
async fn walk_facets(
    state: &ServerState,
    reader: &SecurityContext,
    query: &str,
) -> (Vec<serde_json::Value>, bool) {
    let mut groups = Vec::new();
    let mut in_sql = true;
    let mut token: Option<String> = None;
    loop {
        let page_query = match &token {
            Some(token) => format!("{query}&$top=3&$skiptoken={token}"),
            None => format!("{query}&$top=3"),
        };
        let Ok(page) = facet_page(state, reader, &page_query).await else {
            panic!("{page_query}: refused");
        };
        assert!(page.groups.len() <= 3, "{page_query}");
        in_sql &= page.grouped_in_sql;
        groups.extend(page.groups);
        match page.next_skiptoken {
            Some(next) => token = Some(next),
            None => break,
        }
        assert!(groups.len() <= ROWS, "{query}: the cursor did not move on");
    }
    (groups, in_sql)
}

/// A value as the answer writes it: a number as a double, integral ones as
/// integers; no value is null.
fn written(value: Option<serde_json::Value>) -> serde_json::Value {
    match value {
        None => serde_json::Value::Null,
        Some(serde_json::Value::Number(number)) => {
            let double = number.as_f64().unwrap_or_default();
            if double.fract() == 0.0 && double.abs() <= 9_007_199_254_740_992.0 {
                serde_json::json!(double as i64)
            } else {
                serde_json::json!(double)
            }
        }
        Some(other) => other,
    }
}

fn rank(value: &serde_json::Value) -> u8 {
    match value {
        serde_json::Value::Bool(_) => 0,
        serde_json::Value::Number(_) => 1,
        serde_json::Value::String(_) => 2,
        serde_json::Value::Null => 4,
        _ => 3,
    }
}

fn value_order(a: &serde_json::Value, b: &serde_json::Value) -> Ordering {
    rank(a).cmp(&rank(b)).then_with(|| match (a, b) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => a
            .as_f64()
            .partial_cmp(&b.as_f64())
            .unwrap_or(Ordering::Equal),
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a.cmp(b),
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a.cmp(b),
        _ => a.to_string().cmp(&b.to_string()),
    })
}

/// The groups the evaluator's records give, in the order asked for.
fn expected(
    bodies: &[serde_json::Value],
    property: &str,
    filter: &str,
    order: &str,
) -> Vec<serde_json::Value> {
    let query = if filter.is_empty() {
        String::new()
    } else {
        format!("$filter={filter}")
    };
    let (ids, _) = in_memory(bodies, &query);
    let mut groups: Vec<(serde_json::Value, usize)> = Vec::new();
    for body in bodies {
        if !ids.contains(&listed_entity_id(body)) {
            continue;
        }
        let value = written(crate::query_eval::resolve_property(body, property));
        match groups.iter_mut().find(|(seen, _)| *seen == value) {
            Some((_, count)) => *count += 1,
            None => groups.push((value, 1)),
        }
    }
    let by_count = order.starts_with("count");
    groups.sort_by(|a, b| {
        let by_value = value_order(&a.0, &b.0);
        if by_count {
            b.1.cmp(&a.1).then(by_value)
        } else {
            by_value
        }
    });
    groups
        .into_iter()
        .map(|(value, count)| serde_json::json!({property: value, "count": count}))
        .collect()
}

/// Every property under every filter, each in one of the orders, and every
/// order for two of them: the groups equal the evaluator's, and the store
/// grouped them when it can and only then.
async fn assert_facets(state: &ServerState, bodies: &[serde_json::Value], grouping_store: bool) {
    let system = SecurityContext::system();
    let mut cases = Vec::new();
    for (i, property) in PROPERTIES.iter().enumerate() {
        for (j, filter) in FILTERS.iter().enumerate() {
            cases.push((*property, *filter, orders(property)[(i + j) % 4].clone()));
        }
    }
    for property in ["Status", "rank"] {
        for order in orders(property) {
            cases.push((property, "", order));
        }
    }
    for (property, filter, order) in cases {
        let query = facet_query(property, filter, &order);
        let (groups, in_sql) = walk_facets(state, &system, &query).await;
        assert_eq!(
            groups,
            expected(bodies, property, filter, &order),
            "{query}"
        );
        let sql_expected =
            grouping_store && SQL_KEYS.contains(&property) && SQL_FILTERS.contains(&filter);
        assert_eq!(in_sql, sql_expected, "{query}: grouped in SQL");
    }
}

#[tokio::test]
async fn facets_equal_the_evaluators_groups_on_turso() {
    let (state, bodies, _dir) = turso_cards("facets-turso").await;
    assert_facets(&state, &bodies, true).await;
}

#[tokio::test]
async fn facets_give_the_same_groups_in_memory() {
    let (state, bodies) = sim_cards("facets-sim").await;
    assert_facets(&state, &bodies, false).await;
}

/// Gated on DATABASE_URL; the cards live in a schema of their own.
#[test]
fn facets_equal_the_evaluators_groups_on_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let cards = PostgresCards::seed(&database_url, "facets-postgres").await;
        assert_facets(&cards.state, &cards.bodies, true).await;
        cards.drop().await;
    });
}

/// A reader whose policy hides half the cards counts only the cards it may
/// read: no hidden status is a group and no hidden card is counted, so no
/// group or cursor tells it anything about them.
#[tokio::test]
async fn a_facet_counts_only_the_rows_its_reader_may_read() {
    let (state, bodies, _dir) = turso_cards("facets-reader").await;
    let reader = install_reader(&state);
    let seen = visible(&bodies);
    for (property, filter) in [("Status", ""), ("rank", ""), ("kind", "rank gt 2")] {
        for order in orders(property) {
            let query = facet_query(property, filter, &order);
            let (groups, in_sql) = walk_facets(&state, &reader, &query).await;
            assert_eq!(groups, expected(&seen, property, filter, &order), "{query}");
            assert!(!in_sql, "{query}: each row is judged");
        }
    }
    let (statuses, _) = walk_facets(&state, &reader, &facet_query("Status", "", "")).await;
    let statuses: Vec<&str> = statuses
        .iter()
        .filter_map(|group| group["Status"].as_str())
        .collect();
    assert_eq!(statuses, vec!["Open", "Posted"]);
}

#[tokio::test]
async fn a_facet_read_refuses_what_it_does_not_answer() {
    let (state, _bodies) = sim_cards("facets-refusals").await;
    let system = SecurityContext::system();
    for query in [
        "$apply=filter(rank gt 2)",
        "$apply=groupby((kind,title),aggregate($count as count))",
        "$apply=groupby((kind),aggregate(rank with sum as total))",
        "$apply=groupby((kind))",
    ] {
        assert!(parse_query_options(query).is_err(), "{query}");
    }
    let kind = facet_query("kind", "", "");
    for query in [
        format!("{kind}&$select=kind"),
        format!("{kind}&$expand=Owner"),
        format!("{kind}&$count=true"),
        format!("{kind}&$skip=1"),
        format!("{kind}&$search=apple"),
        format!("{kind}&$orderby=kind desc"),
        format!("{kind}&$orderby=count asc"),
        format!("{kind}&$orderby=title asc"),
        facet_query("nope", "", ""),
        "$apply=groupby((kind),aggregate($count as kind))".to_string(),
    ] {
        assert!(
            matches!(
                facet_page(&state, &system, &query).await,
                Err(QueryPlaneReadError::Unsupported(_))
            ),
            "{query}"
        );
    }
    let title_page = facet_page(
        &state,
        &system,
        &format!("{}&$top=1", facet_query("title", "", "")),
    )
    .await
    .unwrap_or_else(|_| panic!("a title facet"));
    let title_cursor = title_page.next_skiptoken.expect("more titles");
    for token in ["bogus".to_string(), title_cursor] {
        assert!(
            matches!(
                facet_page(&state, &system, &format!("{kind}&$skiptoken={token}")).await,
                Err(QueryPlaneReadError::InvalidContinuation)
            ),
            "{token}"
        );
    }
}

async fn authenticate_as_system(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::system(),
        ));
    next.run(request).await
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).expect("a request"))
        .await
        .expect("a response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("a body");
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

/// Over HTTP: the answer's shape, its next link, and the 400s.
#[tokio::test]
async fn a_facet_read_over_http_pages_by_its_next_link() {
    let (state, bodies, _dir) = turso_cards("facets-http").await;
    let app =
        crate::router::build_router(state).layer(axum::middleware::from_fn(authenticate_as_system));
    let apply = "$apply=groupby((Status),aggregate($count%20as%20n))";
    let (status, first) = get(&app, &format!("/tdata/Cards?{apply}&$top=2")).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let in_status = |status: &str| {
        bodies
            .iter()
            .filter(|body| body["status"] == status)
            .count()
    };
    assert_eq!(
        first["value"],
        serde_json::json!([
            {"Status": "Done", "n": in_status("Done")},
            {"Status": "Held", "n": in_status("Held")}
        ])
    );
    let next = first["@odata.nextLink"].as_str().expect("a next link");
    let (status, second) = get(&app, &format!("/tdata/{next}")).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        second["value"].as_array().map(Vec::len),
        Some(2),
        "{second}"
    );
    assert!(second.get("@odata.nextLink").is_none(), "{second}");
    for refused in [
        format!("/tdata/Cards?{apply}&actions=true"),
        format!("/tdata/Cards?{apply}&$select=kind"),
        "/tdata/Cards?$apply=aggregate($count%20as%20n)".to_string(),
    ] {
        let (status, body) = get(&app, &refused).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}: {body}");
    }
}
