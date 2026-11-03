//! A timer's move that changes nothing writes nothing, and its timer keeps
//! its pace (`entity_actor::unrecorded`).
//!
//! The poller's `Poll` repeats every [`POLL`] and changes nothing; its trigger
//! is a relay's `Pass`, a reaction's move that changes nothing either, whose
//! own trigger counts on a tally. So every period the tally's journal gains a
//! `Count`, while the poller's and the relay's gain nothing. A person's `Poll`
//! changes nothing too, and is recorded: they asked for it. A restart arms the
//! poll from the newest unrecorded one, whose timer row it reads.

use std::time::Duration;

use super::server::{SETTLE, Server, World, millis_between, person};

/// `POLLER_IOA`'s and `COUNTING_POLLER_IOA`'s `after_seconds`.
const POLL: Duration = Duration::from_secs(15);

/// How far from a poll's deadline a check looks: before it, nothing new has
/// fired; after it, the poll and everything it set off have.
const MARGIN: Duration = Duration::from_millis(500);

/// How many times `action` appears in `journal`.
fn count(journal: &[String], action: &str) -> usize {
    journal.iter().filter(|entry| *entry == action).count()
}

/// A poller watching since now, with its relay and tally made.
fn watch(server: &Server, id: &str) {
    server.create("Relay", id);
    server.create("Tally", id);
    server.act("Poller", id, "Watch", &person("alice"));
}

pub(crate) fn polls_that_change_nothing_write_nothing_and_still_trigger(world: &World) {
    let id = "quiet-poller";
    let server = Server::start(world, "unrecorded-run-1");
    let t0 = server.now();
    watch(&server, id);
    let watched = server.journal("Poller", id);
    let relayed = server.journal("Relay", id);
    assert_eq!(
        count(&watched, "Watch"),
        1,
        "the person's Watch is recorded"
    );

    for period in 1..=4u32 {
        server.run_until(t0 + POLL * period - MARGIN);
        assert_eq!(
            server.read("Tally", id, "count").0,
            period as usize - 1,
            "a poll that wrote no event re-arms from its own time, not the last \
             event's: it does not fire before its period"
        );
        server.run_until(t0 + POLL * period + MARGIN);
        assert_eq!(
            server.read("Tally", id, "count").0,
            period as usize,
            "each period's poll still sets off its trigger, and its trigger's"
        );
    }
    assert_eq!(
        server.journal("Poller", id),
        watched,
        "a timer's poll that changes nothing writes no event"
    );
    assert_eq!(
        server.journal("Relay", id),
        relayed,
        "a reaction's move that changes nothing writes no event"
    );
    assert_eq!(
        count(&server.journal("Tally", id), "Count"),
        4,
        "the trigger's move that changes its record is written once per period"
    );

    // The same Poll, asked for by a person: it changes nothing, and is recorded.
    server.act("Poller", id, "Poll", &person("bob"));
    let asked = server.journal("Poller", id);
    assert_eq!(
        count(&asked, "Poll"),
        1,
        "a person's move is recorded even when it changes nothing: {asked:?}"
    );
    assert_eq!(server.read("Tally", id, "count").0, 5);
    let stopped = server.now();
    server.kill(world, stopped);
}

pub(crate) fn polls_that_change_the_record_are_recorded(world: &World) {
    let id = "counting-poller";
    let server = Server::start(world, "unrecorded-run-2");
    let t0 = server.now();
    server.act("CountingPoller", id, "Watch", &person("alice"));
    server.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(server.read("CountingPoller", id, "polls").0, 3);
    assert_eq!(
        count(&server.journal("CountingPoller", id), "Poll"),
        3,
        "a timer's poll that changes its record is written, one per period"
    );
    let stopped = server.now();
    server.kill(world, stopped);
}

pub(crate) fn a_restart_arms_the_poll_from_the_newest_unrecorded_one(world: &World) {
    let mut rng = world.rng();
    // Down from just after the third poll until some time before the fourth
    // falls due. A timer armed from the last event (the `Watch`, three
    // periods back) would be overdue at boot.
    let back = millis_between(&mut rng, 1_500, 13_000);
    let id = "restarted-poller";

    let first = Server::start(world, "unrecorded-restart-1");
    let t0 = first.now();
    watch(&first, id);
    let watched = first.journal("Poller", id);
    first.run_until(t0 + POLL * 3 + MARGIN);
    assert_eq!(first.read("Tally", id, "count").0, 3);
    assert!(first.has_row(id), "the poll's timer keeps its row");
    first.kill(world, t0 + POLL * 3 + back);

    let second = Server::start(world, "unrecorded-restart-2");
    second.run_for(SETTLE);
    assert_eq!(
        second.read("Tally", id, "count").0,
        3,
        "the restart arms the poll from the newest unrecorded one's row, so it is not overdue"
    );
    second.run_until(t0 + POLL * 4 - MARGIN);
    assert_eq!(
        second.read("Tally", id, "count").0,
        3,
        "a waiting poll does not fire early"
    );
    second.run_until(t0 + POLL * 4 + MARGIN);
    assert_eq!(
        second.read("Tally", id, "count").0,
        4,
        "the poll fires a period after the newest unrecorded one"
    );
    assert_eq!(
        second.journal("Poller", id),
        watched,
        "no poll wrote an event, before or after the restart"
    );
    let stopped = second.now();
    second.kill(world, stopped);
}
