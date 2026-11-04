//! A poll that re-arms itself with `schedule` and changes nothing writes
//! nothing, and is never lost.
//!
//! The `Looper`'s `Poll` arms the next `Poll` every [`POLL`] and changes
//! nothing; its trigger is a relay's `Pass`, whose own trigger counts on a
//! tally, so every period the tally's journal gains a `Count` while the looper's
//! gains nothing and its row stays as it is. The row's due time is then behind
//! the loop's: a restart finds it past due, looks once at once and keeps the
//! pace from there. A fire that changes the record is written as before, and a
//! loop whose record leaves its state lapses and ends.

use std::time::Duration;

use super::server::{SETTLE, Server, World, millis_between, person};

/// The delay `Poll` and `Tick` schedule themselves at in `LOOPER_IOA`.
const POLL: Duration = Duration::from_secs(15);

/// How far from a poll's deadline a check looks: before it, nothing new has
/// fired; after it, the poll and everything it set off have.
const MARGIN: Duration = Duration::from_millis(500);

/// A looper watching since now, with its relay and tally made.
fn watch(server: &Server, id: &str) {
    server.create("Relay", id);
    server.create("Tally", id);
    server.act("Looper", id, "Watch", &person("alice"));
}

fn looks(server: &Server, id: &str) -> usize {
    server.read("Tally", id, "count").0
}

pub(crate) fn a_loop_that_changes_nothing_writes_nothing_and_keeps_its_pace(world: &World) {
    let id = "quiet-loop";
    let server = Server::start(world, "schedule-loop-run-1");
    let t0 = server.now();
    watch(&server, id);
    let watched = server.journal("Looper", id);
    let relayed = server.journal("Relay", id);
    let arms = server.scheduled_arms(id);
    assert_eq!(arms.len(), 1, "Watch armed one row: {arms:?}");

    for period in 1..=4u32 {
        server.run_until(t0 + POLL * period - MARGIN);
        assert_eq!(
            looks(&server, id),
            period as usize - 1,
            "the next fire waits its period from the fire before it"
        );
        server.run_until(t0 + POLL * period + MARGIN);
        assert_eq!(
            looks(&server, id),
            period as usize,
            "each period's fire runs, each with a key of its own, and sets off its trigger"
        );
    }
    assert_eq!(
        server.journal("Looper", id),
        watched,
        "a fire that changes nothing and arms only itself writes no event"
    );
    assert_eq!(
        server.journal("Relay", id),
        relayed,
        "its reaction that changes nothing writes no event"
    );
    assert_eq!(
        server.scheduled_arms(id),
        arms,
        "the loop goes on under the row it started from"
    );
    let stopped = server.now();
    server.kill(world, stopped);
}

pub(crate) fn a_loop_that_changes_its_record_is_written(world: &World) {
    let id = "counting-loop";
    let server = Server::start(world, "schedule-loop-run-2");
    let t0 = server.now();
    server.act("Looper", id, "StartTicking", &person("alice"));
    let first = server.scheduled_arms(id);
    server.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(server.read("Looper", id, "ticks").0, 3);
    let journal = server.journal("Looper", id);
    assert_eq!(
        journal.iter().filter(|action| *action == "Tick").count(),
        3,
        "a fire that changes its record is written: {journal:?}"
    );
    let arms = server.scheduled_arms(id);
    assert_eq!(arms.len(), 1, "one row waits: {arms:?}");
    assert_ne!(arms, first, "each written fire writes the next row");
    let stopped = server.now();
    server.kill(world, stopped);
}

pub(crate) fn a_loop_survives_restarts_and_looks_once_at_each(world: &World) {
    let mut rng = world.rng();
    let id = "restarted-loop";

    let first = Server::start(world, "schedule-loop-restart-1");
    let t0 = first.now();
    watch(&first, id);
    let watched = first.journal("Looper", id);
    let arms = first.scheduled_arms(id);
    first.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(looks(&first, id), 3);
    // Down from just after the third poll until some time before the fourth
    // falls due: the row still says the first poll's time.
    let back_up = t0 + POLL * 3 + millis_between(&mut rng, 1_500, 13_000);
    first.kill(world, back_up);

    let second = Server::start(world, "schedule-loop-restart-2");
    second.run_for(SETTLE);
    assert_eq!(
        looks(&second, id),
        4,
        "the restart finds the row past due, and the loop looks once at once"
    );
    second.run_until(back_up + POLL - MARGIN);
    assert_eq!(
        looks(&second, id),
        4,
        "that look armed the next: it does not fire again early"
    );
    second.run_until(back_up + POLL + SETTLE + MARGIN);
    assert_eq!(looks(&second, id), 5, "the loop goes on, a period later");

    // Down for several periods: the missed polls are not replayed.
    let away = second.now() + POLL * 3 + millis_between(&mut rng, 1_000, 10_000);
    second.kill(world, away);
    let third = Server::start(world, "schedule-loop-restart-3");
    third.run_for(SETTLE);
    assert_eq!(looks(&third, id), 6, "one look, not one per missed period");
    third.run_until(away + POLL + SETTLE + MARGIN);
    assert_eq!(looks(&third, id), 7, "and the loop is still going");

    assert_eq!(
        third.journal("Looper", id),
        watched,
        "no fire wrote an event, before or after a restart"
    );
    assert_eq!(
        third.scheduled_arms(id),
        arms,
        "no restart lost the row or made a second one"
    );
    let stopped = third.now();
    third.kill(world, stopped);
}

pub(crate) fn a_loop_goes_on_past_other_moves_and_ends_with_its_state(world: &World) {
    let id = "moved-loop";
    let server = Server::start(world, "schedule-loop-run-3");
    let t0 = server.now();
    watch(&server, id);
    let arms = server.scheduled_arms(id);
    server.run_until(t0 + POLL * 2 + MARGIN);
    assert_eq!(looks(&server, id), 2);

    // A move that writes, between fires, leaves the loop's clock and its row.
    server.act("Looper", id, "Note", &person("bob"));
    assert_eq!(server.read("Looper", id, "notes").0, 1);
    assert_eq!(
        server.scheduled_arms(id),
        arms,
        "another move does not touch the loop's row"
    );
    server.run_until(t0 + POLL * 3 - MARGIN);
    assert_eq!(looks(&server, id), 2, "and does not bring its fire forward");
    server.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(looks(&server, id), 3, "and does not put it off");

    // Leaving the state ends the loop: its next fire lapses and removes the row.
    server.act("Looper", id, "Stop", &person("bob"));
    assert_eq!(
        server.scheduled_arms(id),
        arms,
        "the row waits for the fire it is due"
    );
    server.run_until(t0 + POLL * 4 + MARGIN);
    assert_eq!(
        looks(&server, id),
        3,
        "the fire due after Stop does nothing"
    );
    assert!(
        server.scheduled_arms(id).is_empty(),
        "and nothing is left to fire at the next boot"
    );
    server.run_until(t0 + POLL * 6 + MARGIN);
    assert_eq!(looks(&server, id), 3, "the loop has ended");
    let stopped = server.now();
    server.kill(world, stopped);
}
