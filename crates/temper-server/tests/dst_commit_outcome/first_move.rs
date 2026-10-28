//! A move on a record that does not exist yet creates it. The record's
//! `Created` event and the move are one commit, so a process that stops
//! partway leaves either no record or the record with its move: never a
//! record created without the move that created it.

use super::*;

/// First moves per seed, each on a record of its own.
const FIRST_MOVES: usize = 4;

/// Per seed: first moves (`Bump` or `Hold`) on records that do not exist,
/// each with the process stopped after none of its commits, after one, or
/// not at all. Then the record's journal is empty or holds the move; with no
/// stop the move is one commit, and the record's rows follow its journal.
#[test]
fn dst_a_first_move_commits_its_record() {
    let mut broken = Vec::new();
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime builds");
        if let Err(report) = runtime.block_on(first_moves(seed)) {
            broken.push(report);
        }
    }
    assert!(
        broken.is_empty(),
        "invariants broken on {} of {SEEDS} seeds:\n\n{}",
        broken.len(),
        broken.join("\n\n")
    );
}

async fn first_moves(seed: u64) -> Result<(), String> {
    let store = SimEventStore::no_faults(seed);
    let state = server(&store, seed);
    let mut rng = DeterministicRng::new(seed ^ 0xf1_25_7e);
    let mut run = Run {
        seed,
        trace: Vec::new(),
        noted: BTreeMap::new(),
    };
    for n in 0..FIRST_MOVES {
        let id = format!("new-{n}");
        let journal_id = journal_of(&id);
        let action = if rng.next_bound(2) == 0 {
            "Bump"
        } else {
            "Hold"
        };
        let mark = format!("m-{n}");
        // The process stops after this many of the move's commits.
        let stop = match rng.next_bound(3) {
            0 => Some(0),
            1 => Some(1),
            _ => None,
        };
        store.stop_appends_after(&journal_id, stop);
        let answer = move_once(&state, &store, &id, action, &mark, None).await;
        settle().await;
        let events = store
            .read_events(&journal_id, 0)
            .await
            .expect("the journal reads");
        let kinds: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
        run.trace.push(format!(
            "{mark}: {action} {id}, stop after {stop:?} commits: {answer:?}, journal {kinds:?}"
        ));
        let moves = landed(&store, &id, &mark).await;
        if !events.is_empty() && moves != 1 {
            return Err(run.fail(
                "a record exists only with its first move",
                format!("{id}: journal {kinds:?}, the move landed {moves} times"),
            ));
        }
        if stop.is_none() {
            let commits = store.commits(&journal_id);
            if commits != 1 {
                return Err(run.fail(
                    "a first move is one commit",
                    format!("{mark} took {commits} commits"),
                ));
            }
            let (journal, _) = journal(&store, &id).await;
            if let Err((invariant, detail)) = rows::rows_follow_journal(&store, &id, &journal) {
                return Err(run.fail(invariant, detail));
            }
        }
        // The process is back: nothing more for this record.
        store.stop_appends_after(&journal_id, None);
    }
    Ok(())
}
