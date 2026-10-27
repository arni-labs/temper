//! The database has one writer. Writes that arrive together take turns at
//! the store's write gate, where a write waits its turn, and not at the
//! database's write lock, where one that waits past its lock wait is refused
//! and its move is lost.

use std::time::{Duration, Instant};

use temper_runtime::persistence::EventStore;

use super::super::commit_probe::{CommitPoint, CommitProbe};
use super::{make_store, test_envelope};

/// How long a write waits for the database's write lock before its move is
/// refused (the store's default append attempt timeout).
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// One commit on a busy drive: the Factory's statements took 1.0 to 1.9 s on
/// its Tensorlake drive under load (Datadog, roll3, 26 Oct 2026).
const SLOW_COMMIT: Duration = Duration::from_secs(2);

/// Moves that arrive together: enough that the last one's turn comes after
/// [`LOCK_WAIT`] has passed.
const WRITES: u32 = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn moves_that_arrive_together_on_a_slow_drive_all_land() {
    let store = make_store("write-queue")
        .await
        .with_commit_probe(CommitProbe::every(
            CommitPoint::Wrote,
            SLOW_COMMIT,
            LOCK_WAIT,
        ));
    let started = Instant::now();
    let moves: Vec<_> = (0..WRITES)
        .map(|n| {
            let store = store.clone();
            tokio::spawn(async move {
                let journal = format!("tenant-a:Job:job-{n}");
                let answer = store
                    .append(
                        &journal,
                        0,
                        &[test_envelope("Created", serde_json::json!({}))],
                    )
                    .await;
                (n, answer, started.elapsed())
            })
        })
        .collect();
    let mut refused = Vec::new();
    for answer in moves {
        let (n, answer, at) = answer.await.expect("move task");
        eprintln!("move {n}: {answer:?} after {} ms", at.as_millis());
        if let Err(error) = answer {
            refused.push(format!("move {n} after {} ms: {error}", at.as_millis()));
        }
    }
    assert!(
        refused.is_empty(),
        "{} of {WRITES} moves were refused, though the database finished every \
         write within {:?}:\n{}",
        refused.len(),
        SLOW_COMMIT * WRITES,
        refused.join("\n")
    );
    for n in 0..WRITES {
        let events = store
            .read_events(&format!("tenant-a:Job:job-{n}"), 0)
            .await
            .expect("read");
        assert_eq!(events.len(), 1, "move {n} landed once");
    }
}
