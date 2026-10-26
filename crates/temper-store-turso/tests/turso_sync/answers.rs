//! An answer waits only for its own push, never for the change log's trim,
//! however long the confirmed part of the log has grown.

use std::time::{Duration, Instant};

use super::support::*;

/// The time an answer has for its push (temper-cli's `ANSWER_BUDGET`).
const ANSWER_BUDGET: Duration = Duration::from_secs(10);

#[tokio::test]
async fn no_answer_waits_on_a_trim() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let (a, copy) = confirmed_log(&server, &path, BIG_LOG).await;

    write(&a, "answered", 0).await;
    let rows = copy.change_log_rows().await.expect("rows");
    let started = Instant::now();
    copy.push_committed_within(ANSWER_BUDGET)
        .await
        .expect("the answer's push");
    let took = started.elapsed();

    let left = copy.change_log_rows().await.expect("rows");
    assert_eq!(
        left,
        rows,
        "an answer's push deleted {} confirmed change log rows",
        rows - left
    );
    assert!(took < ANSWER_BUDGET, "an answer's push took {took:?}");

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(
        count(&b, "answered").await,
        1,
        "the answer's change reached the remote"
    );
}
