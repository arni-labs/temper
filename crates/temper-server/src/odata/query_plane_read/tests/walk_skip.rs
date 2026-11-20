//! A `$skip` on a walked read answers the page the evaluator gives and holds
//! only that page (ADR-0188). Before, the walk kept every row up to
//! `$skip + $top` as documents, and a sorted walk sorted them all again after
//! each batch, so `$skip=100000000` on a large type held every document.
//!
//! `up` holds numbers, a string, explicit JSON nulls (every 13th card from
//! the 4th) and no value, so its order crosses the walk's 20-row batches with
//! nulls on both sides: a batch-wise sort and a whole sort agree only when
//! the order is total.

use super::large_set::{BUDGET, ROWS, in_memory, install_reader, read, sim_cards, turso_cards};
use super::*;

/// A batch being judged and the page kept with its probe row, whatever
/// `$skip` is.
const MOST_HELD: usize = 2 * BUDGET.max_entities + 1;

/// Read every page of `query` by `$skip`, and check each against the
/// evaluator's rows `skip..skip + 20` and what it held.
async fn assert_skips(
    state: &ServerState,
    reader: &SecurityContext,
    bodies: &[serde_json::Value],
    query: &str,
    strategy: QueryPlaneReadStrategy,
) {
    let (want, _) = in_memory(bodies, query);
    let skips = (0..=ROWS + 20)
        .step_by(20)
        .chain([7, 151, ROWS - 1, 100_000_000]);
    for skip in skips {
        let page_query = format!("{query}&$skip={skip}&$top=20");
        let page = read(state, reader, &page_query).await;
        let got: Vec<String> = page.entities.iter().map(listed_entity_id).collect();
        let start = skip.min(want.len());
        let end = skip.saturating_add(20).min(want.len());
        assert_eq!(got, want[start..end], "{page_query}: the evaluator's page");
        assert_eq!(page.telemetry.strategy, strategy, "{page_query}: walked");
        assert!(
            page.telemetry.held_documents <= MOST_HELD,
            "{page_query}: held {} documents",
            page.telemetry.held_documents
        );
    }
}

/// Reads the simulation store walks from its id list: in id order, and in
/// an order the walk sorts itself.
const SIM_WALKS: [&str; 4] = [
    "",
    "$orderby=up asc",
    "$orderby=up desc",
    "$filter=contains(title,'p')&$orderby=pinned desc,up asc",
];

#[tokio::test]
async fn a_skip_on_a_walk_from_the_id_list_answers_its_page_in_memory() {
    let (state, bodies) = sim_cards("walk-skip-sim").await;
    let system = SecurityContext::system();
    for query in SIM_WALKS {
        assert_skips(
            &state,
            &system,
            &bodies,
            query,
            QueryPlaneReadStrategy::ReadSourceCursor,
        )
        .await;
    }
    let reader = install_reader(&state);
    let seen = super::large_set::visible(&bodies);
    for query in SIM_WALKS {
        assert_skips(
            &state,
            &reader,
            &seen,
            query,
            QueryPlaneReadStrategy::ReadSourceCursor,
        )
        .await;
    }
}

/// On Turso a reader's unordered list is the store's walk, page by page; an
/// order by `up`, which holds a value of another type, is sorted by the
/// walk from the id list.
#[tokio::test]
async fn a_skip_on_each_walk_answers_its_page_on_turso() {
    let (state, bodies, _dir) = turso_cards("walk-skip-turso").await;
    let system = SecurityContext::system();
    assert_skips(
        &state,
        &system,
        &bodies,
        "$filter=contains(title,'p')",
        QueryPlaneReadStrategy::NativePagePushdown,
    )
    .await;
    for query in ["$orderby=up asc", "$orderby=up desc"] {
        assert_skips(
            &state,
            &system,
            &bodies,
            query,
            QueryPlaneReadStrategy::ReadSourceCursor,
        )
        .await;
    }
    let reader = install_reader(&state);
    let seen = super::large_set::visible(&bodies);
    assert_skips(
        &state,
        &reader,
        &seen,
        "",
        QueryPlaneReadStrategy::NativePagePushdown,
    )
    .await;
    assert_skips(
        &state,
        &reader,
        &seen,
        "$orderby=up desc",
        QueryPlaneReadStrategy::ReadSourceCursor,
    )
    .await;
}
