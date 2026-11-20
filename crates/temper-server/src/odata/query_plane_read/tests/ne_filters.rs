//! `ne` on the status and on a declared app field is exact in SQL, so a read
//! with it walks and counts a type larger than the scan budget (ADR-0188).
//! Before, `ne` was never exact: the read fell to the bounded scan and every
//! one of these answered 413.

use super::large_set::{PostgresCards, assert_matches_memory, sim_cards, turso_cards};
use super::*;

const NE_READS: [&str; 6] = [
    "$filter=Status ne 'Done'",
    "$filter=kind ne 'steer'",
    "$filter=kind ne ''",
    "$filter=pinned ne true",
    "$filter=kind ne 'steer' and Status ne 'Held'",
    "$filter=Status ne 'Done'&$orderby=Status desc",
];

#[tokio::test]
async fn ne_reads_walk_and_count_past_the_budget_on_turso() {
    let (state, bodies, _dir) = turso_cards("ne-turso").await;
    let system = SecurityContext::system();
    for query in NE_READS {
        assert_matches_memory(&state, &system, &bodies, query).await;
    }
}

#[tokio::test]
async fn ne_reads_give_the_same_pages_in_memory() {
    let (state, bodies) = sim_cards("ne-sim").await;
    let system = SecurityContext::system();
    for query in NE_READS {
        assert_matches_memory(&state, &system, &bodies, query).await;
    }
}

/// Gated on DATABASE_URL; the cards live in a schema of their own.
#[test]
fn ne_reads_walk_and_count_past_the_budget_on_postgres() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let cards = PostgresCards::seed(&database_url, "ne-postgres").await;
        let system = SecurityContext::system();
        for query in NE_READS {
            assert_matches_memory(&cards.state, &system, &cards.bodies, query).await;
        }
        cards.drop().await;
    });
}
