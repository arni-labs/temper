//! Recovery cases of the restart tests: timers with no row, timers that
//! already fired, many overdue timers at boot, and a fire whose row survived.
use super::*;

/// A timer with no row (armed before rows were kept) comes back on its
/// record's next action and counts the time already spent in its state, even
/// when the entry into the state is no longer among the retained events (the
/// window keeps 50; a snapshot keeps none). Before the status clock it was
/// armed for a full budget on every such hydration, so a daily timer on a
/// server restarted more often than daily never fired (Inspiration Bank's
/// feeds, 15 to 20 Oct 2026).
#[test]
fn a_timer_without_a_row_counts_the_time_already_spent_in_its_state() {
    let (_dir, backend) = turso();
    let first = Server::start(&backend, "clock-run-1");
    let t0 = Instant::now();
    first.act("Probe", "noted", "Arm", &person("alice"));
    for _ in 0..55 {
        first.act("Probe", "noted", "Note", &person("alice"));
    }
    first.forget_rows();
    assert_eq!(
        first.read("Probe", "noted", "fires").0,
        0,
        "armed and waiting"
    );
    first.kill();

    // 4.5 s into a state with a 4 s timeout.
    sleep_until(t0 + Duration::from_millis(4_500));
    let second = Server::start(&backend, "clock-run-2");
    assert_eq!(
        second.read("Probe", "noted", "fires").0,
        0,
        "no row: nothing armed at boot"
    );
    second.act("Probe", "noted", "Note", &person("bob"));
    assert_eq!(
        second.wait_for("Probe", "noted", "fires", 1, Duration::from_secs(1)),
        1,
        "overdue: the next action's re-arm fires it at once"
    );
    second.kill();
}

/// A timer whose action keeps the record in its state (and does not reset the
/// clock) fires once: after a restart, the record's next action finds it
/// already fired and does not arm it again.
#[test]
fn a_timer_that_fired_does_not_fire_again_after_a_restart() {
    let (_dir, backend) = turso();
    let first = Server::start(&backend, "fired-run-1");
    first.act("Probe", "fired", "Arm", &person("alice"));
    assert_eq!(
        first.wait_for("Probe", "fired", "fires", 1, Duration::from_secs(6)),
        1
    );
    first.kill();

    let second = Server::start(&backend, "fired-run-2");
    second.act("Probe", "fired", "Note", &person("bob"));
    std::thread::sleep(Duration::from_secs(1)); // determinism-ok: real-time restart test
    assert_eq!(
        second.read("Probe", "fired", "fires"),
        (1, json!("alice")),
        "it fired once, before the restart"
    );
    second.kill();
}

/// Many timers that fell due while the server was down: at boot every one
/// fires exactly once, as whoever armed it (the boot gate lets a few fire at a
/// time; this checks it loses and repeats none).
#[test]
fn many_overdue_timers_each_fire_once_after_restart() {
    let (_dir, backend) = turso();
    let ids: Vec<String> = (0..40).map(|i| format!("overdue-{i}")).collect();
    let first = Server::start(&backend, "burst-run-1");
    let t0 = Instant::now();
    first.arm_all("Probe", &ids, &person("alice"));
    // Each row is written before its Arm returns; a timer that already fired
    // has ended and left none.
    let waiting = ids
        .iter()
        .filter(|id| first.read("Probe", id, "fires").0 == 0)
        .count();
    first.kill();
    assert!(waiting > 0, "every timer fired before the restart");

    sleep_until(t0 + Duration::from_millis(4_500));
    let second = Server::start(&backend, "burst-run-2");
    for id in &ids {
        assert_eq!(
            second.wait_for("Probe", id, "fires", 1, Duration::from_secs(10)),
            1,
            "{id} fires after the restart"
        );
    }
    std::thread::sleep(Duration::from_secs(1)); // determinism-ok: real-time restart test
    for id in &ids {
        assert_eq!(
            second.read("Probe", id, "fires"),
            (1, json!("alice")),
            "{id}"
        );
    }
    second.kill();
}

/// The fire is a new action: the arming request's idempotency key, which the
/// entity has already recorded, must not make it a duplicate that never runs.
#[test]
fn timer_armed_by_a_request_with_an_idempotency_key_fires() {
    let (_dir, backend) = turso();
    let server = Server::start(&backend, "keyed-arm");
    let keyed = AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            "request-1".to_string(),
        )),
        ..person("alice")
    };
    server.act("Probe", "keyed", "Arm", &keyed);
    assert_eq!(
        server.wait_for("Probe", "keyed", "fires", 1, Duration::from_secs(6)),
        1
    );
    assert_eq!(server.read("Probe", "keyed", "fires").1, json!("alice"));
    server.kill();
}

/// A fire that committed before a crash left its row behind (its delete never
/// happened): the next boot arms that row again, finds the action already ran
/// since the clock started, and ends the timer without firing.
#[test]
fn a_fire_whose_row_survived_runs_once_after_a_restart() {
    let (_dir, backend) = turso();
    let first = Server::start(&backend, "kept-row-1");
    first.act("Probe", "kept", "Arm", &person("alice"));
    let row = first
        .rows()
        .into_iter()
        .find(|row| row.entity_id == "kept")
        .expect("the arm's row");
    assert_eq!(
        first.wait_for("Probe", "kept", "fires", 1, Duration::from_secs(6)),
        1
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while first.rows().iter().any(|row| row.entity_id == "kept") {
        assert!(Instant::now() < deadline, "the fired timer's row stays");
        std::thread::sleep(Duration::from_millis(20)); // determinism-ok: real-time restart test
    }
    first.restore_row(&row);
    first.kill();

    let second = Server::start(&backend, "kept-row-2");
    let deadline = Instant::now() + Duration::from_secs(5);
    while second.rows().iter().any(|row| row.entity_id == "kept") {
        assert!(Instant::now() < deadline, "the boot did not take the row");
        std::thread::sleep(Duration::from_millis(20)); // determinism-ok: real-time restart test
    }
    assert_eq!(
        second.read("Probe", "kept", "fires"),
        (1, json!("alice")),
        "the boot took the row and ended it; the action ran once"
    );
    second.kill();
}
