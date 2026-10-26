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

/// A ready local DELETE must give the single-threaded answer task a turn.
#[tokio::test(flavor = "current_thread")]
async fn background_trim_leaves_room_for_answers() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    // This tests scheduling, not backlog capacity (the separate long-log
    // tests retain 300,000 rows). Keep setup cheap on slower CI runners;
    // assert a backlog remains so exhaustion cannot satisfy the timings.
    let (a, copy) = confirmed_log(&server, &path, 30_000).await;
    let tick = Duration::from_secs(1);

    let started = Instant::now();
    let idle_rows = copy.trim_pushed(tick, || false).await.expect("idle trim");
    let idle = started.elapsed();
    assert!(
        idle >= tick,
        "idle trim should use the whole tick: {idle:?}"
    );
    assert!(
        idle < tick + Duration::from_millis(250),
        "idle batch overran: {idle:?}"
    );
    eprintln!("idle: {idle_rows} rows in {} ms", idle.as_millis());

    assert!(
        copy.change_log_rows().await.expect("rows") > 1,
        "idle backlog exhausted"
    );

    let in_flight = AtomicBool::new(false);
    let started = Instant::now();
    let trim = async {
        let rows = copy
            .trim_pushed(tick, || in_flight.load(Ordering::Acquire))
            .await
            .expect("busy trim");
        (rows, started.elapsed())
    };
    let arrival = tokio::time::sleep(Duration::from_millis(125));
    let answer = async {
        // Arrive mid-call and stay in flight past the floor.
        arrival.await;
        let arrived = started.elapsed();
        in_flight.store(true, Ordering::Release);
        write(&a, "overlap", 0).await;
        let write_wait = started.elapsed() - arrived;
        assert!(
            write_wait < Duration::from_millis(200),
            "answer write waited longer than about one batch: {write_wait:?}"
        );
        copy.push_committed_within(ANSWER_BUDGET)
            .await
            .expect("overlapping push");
        arrived
    };
    let ((busy_rows, busy), arrived) = tokio::join!(trim, answer);
    assert!(
        arrived < Duration::from_millis(250),
        "answer task starved: {arrived:?}"
    );
    assert!(
        busy >= Duration::from_millis(250),
        "trim lost its floor: {busy:?}"
    );
    assert!(
        busy < Duration::from_millis(450),
        "busy trim failed to leave room: {busy:?}"
    );
    eprintln!("busy: {busy_rows} rows in {} ms", busy.as_millis());

    // A request already held in flight gets the same guaranteed floor.
    let started = Instant::now();
    copy.trim_pushed(tick, || true)
        .await
        .expect("held request trim");
    let held = started.elapsed();
    assert!(held >= Duration::from_millis(250), "floor: {held:?}");
    assert!(held < Duration::from_millis(450), "held request: {held:?}");

    assert!(
        copy.change_log_rows().await.expect("rows") > 1,
        "busy backlog exhausted"
    );

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "overlap").await, 1, "overlapping change copied");
}
