//! A comparison of a declared app field with a literal of its own type is
//! made in SQL on the row's typed JSON value, so its pages and its count are
//! the evaluator's and need no row-by-row recheck (ADR-0188). Before, a
//! range or a number was never pushed down and past the scan budget the read
//! answered 413, and a string or boolean equality read the field index's
//! text, which also holds a number or a string with the same text.

use super::large_set::{
    BUDGET, PostgresCards, assert_matches_memory, read, sim_cards, turso_cards,
};
use super::*;

const COMPARISONS: [&str; 10] = [
    "$filter=rank gt 5",
    "$filter=said_at ge '2026-11-01' and said_at lt '2026-12-01'",
    "$filter=kind eq 'steer' and Status eq 'Posted'",
    "$filter=up ne 0",
    "$filter=up eq 0",
    "$filter=kind eq '5'",
    "$filter=pinned eq true",
    "$filter=score le 0.30000000000000004 and rank le 3",
    "$filter=title lt 'b' and Status ne 'Held'",
    "$filter=5 le rank and up ne 1&$orderby=rank desc",
];

/// Each page is the store's, judging no more rows than it returns and its
/// probe row, and a count is one SQL count.
async fn assert_exact_in_sql(state: &ServerState, query: &str) {
    let system = SecurityContext::system();
    let counted = read(state, &system, &format!("{query}&$top=0&$count=true")).await;
    assert_eq!(
        counted.telemetry.candidate_count, 0,
        "{query}: counted in SQL"
    );
    let mut token: Option<String> = None;
    loop {
        let page_query = match &token {
            Some(token) => format!("{query}&$top=20&$skiptoken={token}"),
            None => format!("{query}&$top=20"),
        };
        let page = read(state, &system, &page_query).await;
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

async fn assert_comparisons(state: &ServerState, bodies: &[serde_json::Value], in_sql: bool) {
    let system = SecurityContext::system();
    for query in COMPARISONS {
        assert_matches_memory(state, &system, bodies, query).await;
        if in_sql {
            assert_exact_in_sql(state, query).await;
        }
    }
}

#[tokio::test]
async fn comparisons_are_exact_in_sql_on_turso() {
    let (state, bodies, _dir) = turso_cards("comparisons-turso").await;
    assert_comparisons(&state, &bodies, true).await;
}

#[tokio::test]
async fn comparisons_give_the_same_answers_in_memory() {
    let (state, bodies) = sim_cards("comparisons-sim").await;
    assert_comparisons(&state, &bodies, false).await;
}

/// Gated on DATABASE_URL; the cards live in a schema of their own.
#[test]
fn comparisons_are_exact_in_sql_on_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let cards = PostgresCards::seed(&database_url, "comparisons-postgres").await;
        assert_comparisons(&cards.state, &cards.bodies, true).await;
        cards.drop().await;
    });
}
