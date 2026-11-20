//! An order by a declared app field resumes in SQL, page after page, in the
//! order the in-memory sort gives (ADR-0188). Before, its continuation was
//! folded into the filter only, so each later page judged every row before
//! it, and past the scan budget the read answered 413.

use super::large_set::{
    BUDGET, PostgresCards, assert_matches_memory, read, sim_cards, turso_cards,
};
use super::*;

/// Orders by a string, an Int64 and a date kept as text, both ways, with
/// ties broken by the id, with and without a status filter; and a decimal.
const FIELD_ORDERS: [&str; 10] = [
    "$orderby=title asc",
    "$orderby=title desc",
    "$orderby=rank asc",
    "$orderby=rank desc",
    "$orderby=said_at asc",
    "$orderby=said_at desc",
    "$filter=Status eq 'Posted'&$orderby=rank desc",
    "$filter=Status ne 'Done'&$orderby=title asc,rank desc",
    "$filter=Status eq 'Open'&$orderby=said_at desc,title asc",
    "$orderby=score desc,title asc",
];

/// Every page of `query` after the first is read from its cursor: it judges
/// no more rows than one page and its probe row.
async fn assert_resumes_in_sql(state: &ServerState, query: &str) {
    let system = SecurityContext::system();
    let mut token: Option<String> = None;
    loop {
        let page_query = match &token {
            Some(token) => format!("{query}&$top=20&$skiptoken={token}"),
            None => format!("{query}&$top=20"),
        };
        let page = read(state, &system, &page_query).await;
        assert_eq!(
            page.telemetry.strategy,
            QueryPlaneReadStrategy::NativePagePushdown,
            "{page_query}: answered by the store's page"
        );
        assert!(
            page.telemetry.candidate_count <= BUDGET.max_entities + 1,
            "{page_query}: judged {} rows",
            page.telemetry.candidate_count
        );
        match page.next_skiptoken {
            Some(next) => token = Some(next),
            None => break,
        }
    }
}

async fn assert_field_orders(state: &ServerState, bodies: &[serde_json::Value], in_sql: bool) {
    let system = SecurityContext::system();
    for query in FIELD_ORDERS {
        assert_matches_memory(state, &system, bodies, query).await;
        if in_sql {
            assert_resumes_in_sql(state, query).await;
        }
    }
}

#[tokio::test]
async fn field_orders_resume_in_sql_on_turso() {
    let (state, bodies, _dir) = turso_cards("field-orders-turso").await;
    assert_field_orders(&state, &bodies, true).await;
}

#[tokio::test]
async fn field_orders_give_the_same_pages_in_memory() {
    let (state, bodies) = sim_cards("field-orders-sim").await;
    assert_field_orders(&state, &bodies, false).await;
}

/// Gated on DATABASE_URL; the cards live in a schema of their own.
#[test]
fn field_orders_resume_in_sql_on_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let cards = PostgresCards::seed(&database_url, "field-orders-postgres").await;
        assert_field_orders(&cards.state, &cards.bodies, true).await;
        cards.drop().await;
    });
}

async fn strategy_of(state: &ServerState, query: &str) -> QueryPlaneReadStrategy {
    read(state, &SecurityContext::system(), query)
        .await
        .telemetry
        .strategy
}

/// One card holding a string where `rank` holds numbers: the in-memory sort
/// finds it equal to every number, which no SQL order is, so an order by
/// `rank` is not the store's any more; an order by `title` still is.
#[tokio::test]
async fn a_value_of_another_type_keeps_an_order_out_of_sql() {
    let (state, _bodies, _dir) = turso_cards("field-orders-mistyped").await;
    let strategy = |query: &'static str| strategy_of(&state, query);
    assert_eq!(
        strategy("$orderby=rank asc&$top=5").await,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "Card",
            "card-mistyped",
            serde_json::json!({"rank": "7", "title": "x", "said_at": ""}),
        )
        .await
        .expect("create the mistyped card");
    assert_eq!(
        strategy("$orderby=rank asc&$top=5").await,
        QueryPlaneReadStrategy::ReadSourceCursor
    );
    assert_eq!(
        strategy("$orderby=title asc&$top=5").await,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert_eq!(
        strategy("$filter=Status eq 'Done'&$orderby=rank asc&$top=5").await,
        QueryPlaneReadStrategy::NativePagePushdown,
        "the mistyped card is not a candidate of this filter"
    );
}
