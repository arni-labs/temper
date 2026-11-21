//! A list or a count under a policy that judges each row walks every
//! matching record, however many there are, and never shows a hidden one
//! (ADR-0188). Before, a read that had to judge more candidates than the
//! scan budget answered 413.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::large_set::{
    BUDGET, PostgresCards, ROWS, assert_matches_memory, in_memory, install_reader, read, sim_cards,
    turso_cards, visible,
};
use super::*;

/// Reads whose answers under the reader's policy must equal the evaluator's
/// over the cards the reader may see.
const PER_ROW_READS: [&str; 8] = [
    "",
    "$filter=kind ne 'steer'",
    "$filter=Status ne 'Open'",
    "$filter=title eq 'apple'",
    "$filter=contains(title,'pp')",
    "$orderby=Status desc",
    "$filter=contains(title,'p')&$orderby=Status asc",
    "$filter=(contains(title,'Apple') or contains(kind,'xxxx')) and contains(title,'p')&$orderby=Status asc",
];

/// Reads past the old scan budget that every row is allowed for, with a
/// filter SQL cannot answer exactly, so the rows are still walked.
const ALLOWED_READS: [&str; 2] = [
    "$filter=contains(title,'pp')",
    "$filter=contains(title,'p')&$orderby=Status desc",
];

/// The id a continuation token resumes after: its last key.
fn token_id(token: &str) -> String {
    let bytes = URL_SAFE_NO_PAD.decode(token).expect("token is base64");
    let keys: serde_json::Value = serde_json::from_slice(&bytes).expect("token is JSON");
    keys.as_array()
        .and_then(|keys| keys.last())
        .and_then(|key| key.get(1))
        .and_then(serde_json::Value::as_str)
        .expect("a token ends with the id")
        .to_string()
}

/// Walk `query` as the reader and check that no page, count or token shows a
/// card the reader may not see.
async fn assert_hides(state: &ServerState, reader: &SecurityContext, bodies: &[serde_json::Value]) {
    let seen: std::collections::BTreeSet<String> =
        visible(bodies).iter().map(listed_entity_id).collect();
    let mut token: Option<String> = None;
    let mut listed = 0;
    loop {
        let query = match &token {
            Some(token) => format!("$top=7&$count=true&$skiptoken={token}"),
            None => "$top=7&$count=true".to_string(),
        };
        let page = read(state, reader, &query).await;
        assert_eq!(
            page.count,
            Some(seen.len()),
            "the count holds only seen cards"
        );
        for entity in &page.entities {
            assert!(
                seen.contains(&listed_entity_id(entity)),
                "a hidden card was listed"
            );
        }
        listed += page.entities.len();
        match page.next_skiptoken {
            Some(next) => {
                assert!(
                    seen.contains(&token_id(&next)),
                    "a token names a hidden card"
                );
                token = Some(next);
            }
            None => break,
        }
    }
    assert_eq!(listed, seen.len());
}

async fn assert_per_row_reads(state: &ServerState, bodies: &[serde_json::Value]) {
    let reader = install_reader(state);
    let seen = visible(bodies);
    // Every card is judged, so an unfiltered read judges more rows than the
    // old scan budget allowed.
    assert!(bodies.len() > BUDGET.max_entities * 10);
    for query in PER_ROW_READS {
        assert_matches_memory(state, &reader, &seen, query).await;
    }
    assert_hides(state, &reader, bodies).await;
}

async fn assert_allowed_reads(state: &ServerState, bodies: &[serde_json::Value]) {
    let system = SecurityContext::system();
    for query in ALLOWED_READS {
        assert_matches_memory(state, &system, bodies, query).await;
    }
}

#[tokio::test]
async fn per_row_reads_walk_every_record_on_turso() {
    let (state, bodies, _dir) = turso_cards("per-row-turso").await;
    assert_allowed_reads(&state, &bodies).await;
    assert_per_row_reads(&state, &bodies).await;
}

#[tokio::test]
async fn a_sparse_contains_filter_reads_only_catalog_matches() {
    let (state, bodies, _dir) = turso_cards("sparse-substring").await;
    let reader = install_reader(&state);
    let query = "$filter=(Status eq 'Open' or Status eq 'Done' or Status eq 'Posted') and (contains(kind,'xxxxx') or contains(title,'xxxxx'))&$orderby=Status desc";
    let (want, count) = in_memory(&visible(&bodies), query);
    assert!(count > 0);
    let page = read(&state, &reader, &format!("{query}&$count=true&$top=3")).await;
    assert_eq!(page.count, Some(count));
    assert_eq!(
        page.entities
            .iter()
            .map(listed_entity_id)
            .collect::<Vec<_>>(),
        want[..want.len().min(3)],
    );
    assert!(
        page.telemetry.materialized_count < ROWS / 10,
        "a rare search must avoid materializing nonmatches: {}",
        page.telemetry.materialized_count,
    );

    let key_only = read(
        &state,
        &reader,
        "$filter=contains(title,'title') or contains(kind,'title')&$top=3",
    )
    .await;
    assert!(key_only.entities.is_empty());
    assert_eq!(
        key_only.telemetry.materialized_count, 0,
        "a field name in the JSON is not a matching field value",
    );
}

#[tokio::test]
async fn per_row_reads_walk_every_record_in_memory() {
    let (state, bodies) = sim_cards("per-row-sim").await;
    assert_allowed_reads(&state, &bodies).await;
    assert_per_row_reads(&state, &bodies).await;
}

/// Gated on DATABASE_URL; the cards live in a schema of their own.
#[test]
fn per_row_reads_walk_every_record_on_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let cards = PostgresCards::seed(&database_url, "per-row-postgres").await;
        assert_allowed_reads(&cards.state, &cards.bodies).await;
        assert_per_row_reads(&cards.state, &cards.bodies).await;
        cards.drop().await;
    });
}
