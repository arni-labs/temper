//! A timer no one can run stops, its record says so, and a person's move runs it.

use super::*;

/// A timer that never fires, since its action binds a caller and nothing names
/// one when it is found with no row. Its record says so, once, and a person's
/// move runs it.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_waiting_for_a_caller_says_so() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    on_a_server_that_stops(&store, 3600, |state| async move {
        let tenant = TenantId::default();
        state
            .get_or_create_tenant_entity(&tenant, "Shipment", "s-1", json!({}))
            .await
            .expect("the record is created");
        let started = state
            .dispatch_tenant_action(
                &tenant,
                "Shipment",
                "s-1",
                "Start",
                json!({"parent_type": "", "parent_id": "", "line_id": ""}),
                &AgentContext::system(),
            )
            .await
            .expect("the move is dispatched");
        assert!(started.success, "{:?}", started.error);
    });
    let rows = store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == "s-1")
        .expect("the timer left a row");
    store.delete_state_timer("default", "Shipment", "s-1", &row.arm_id);

    let state = server("after", &store, 1).await;
    let record = until(
        &state,
        "Shipments",
        "s-1",
        "said it needs a person",
        is_stopped,
    )
    .await;
    assert_eq!(record["timer_needs_person"], "Diagnose", "{record}");
    assert_eq!(record["status"], "Staging");
    drop(state);
    let state = server("again", &store, 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await; // determinism-ok: real-time test
    assert_eq!(
        logs.stops("s-1"),
        1,
        "a record already saying so says it once"
    );

    act(&state, "Shipment", "s-1", "Nudge", &person("gil")).await;
    let record = until(&state, "Shipments", "s-1", "fired", |record| {
        record["status"] == "Diagnosed"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "gil");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// A chain no one started: the timed action that binds its caller fires once
/// and is refused. It does not fire again, even after a restart. The record
/// says so until a person's move, whose caller its timer then fires as.
#[tokio::test(flavor = "multi_thread")]
async fn a_timed_action_no_one_can_make_stops_until_a_person_moves() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    on_a_server_that_stops(&store, 1, |state| async move {
        act(&state, "Check", "c-1", "Begin", &AgentContext::system()).await;
        until(
            &state,
            "Checks",
            "c-1",
            "said it needs a person",
            is_stopped,
        )
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    });
    assert_eq!(logs.stops("c-1"), 1, "it fired once and was refused once");
    assert!(
        store
            .list_state_timers("default")
            .iter()
            .all(|row| row.entity_id != "c-1"),
        "the stopped timer keeps no row"
    );

    let state = server("after", &store, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let record = read(&state, "Checks", "c-1").await;
    assert_eq!(record["timer_needs_person"], "Finish", "{record}");
    assert_eq!(record["status"], "Checking");
    assert_eq!(logs.stops("c-1"), 1, "a restart does not fire it again");

    act(&state, "Check", "c-1", "Nudge", &person("cal")).await;
    let record = until(&state, "Checks", "c-1", "finished", |record| {
        record["status"] == "Done"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "cal");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// The line's Rollout: the timed move lands and a reaction to it is refused.
/// On a timer that keeps resetting itself, that is a refusal on every fire.
#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_no_one_can_make_stops_a_repeating_timer() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    on_a_server_that_stops(&store, 1, |state| async move {
        act(&state, "Poller", "p-1", "Begin", &AgentContext::system()).await;
        until(
            &state,
            "Pollers",
            "p-1",
            "said it needs a person",
            is_stopped,
        )
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    });
    let state = server("after", &store, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let record = read(&state, "Pollers", "p-1").await;
    assert_eq!(
        record["fields"]["polls"], 1,
        "one fire, restart or not: {record}"
    );
    assert_eq!(record["timer_needs_person"], "Diagnose", "{record}");
    assert_eq!(logs.stops("p-1"), 1);

    act(&state, "Poller", "p-1", "Nudge", &person("dee")).await;
    let record = until(&state, "Pollers", "p-1", "got stuck", |record| {
        record["status"] == "Stuck"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "dee");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// The same when the refusal comes from an integration's answer: the timer's
/// fire has returned and its next arm is set, and it is still the one stopped.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_after_an_integrations_answer_stops_the_timer_that_asked() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    let state = server("asker", &store, 1).await;
    act(&state, "Asker", "a-1", "Begin", &AgentContext::system()).await;
    until(
        &state,
        "Askers",
        "a-1",
        "said it needs a person",
        is_stopped,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let record = read(&state, "Askers", "a-1").await;
    assert_eq!(record["timer_needs_person"], "Diagnose", "{record}");
    assert!(logs.stops("a-1") >= 1);
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let later = read(&state, "Askers", "a-1").await;
    assert_eq!(
        later["fields"]["polls"], record["fields"]["polls"],
        "it stopped polling: {later}"
    );
}

/// A refusal whose timer is gone stops nothing: the timer that asked fired
/// once and the record moved on to a state whose own timer needs no caller.
/// That timer keeps running, and the record never says it needs anyone.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_refusal_does_not_stop_the_next_states_timer() {
    let store = SimEventStore::no_faults(7);
    let state = server("hopper", &store, 1).await;
    act(&state, "Hopper", "h-1", "Begin", &AgentContext::system()).await;
    until(&state, "Hoppers", "h-1", "heard its answer", |record| {
        record["fields"]["seen"] != ""
    })
    .await;
    let record = until(&state, "Hoppers", "h-1", "kept ticking", |record| {
        record["fields"]["ticks"].as_i64().unwrap_or(0) >= 3
    })
    .await;
    assert_eq!(record["status"], "Waiting");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// A person's move finds a timer armed for no one and keeps it, as it keeps
/// any live timer. Its refusal then finds the person in the journal: it runs
/// as them, and the record never says it needs anyone.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_after_a_persons_move_runs_as_that_person() {
    let store = SimEventStore::no_faults(7);
    let state = server("kept", &store, 1).await;
    act(&state, "Check", "c-2", "Begin", &AgentContext::system()).await;
    act(&state, "Check", "c-2", "Nudge", &person("fay")).await;
    let record = until(&state, "Checks", "c-2", "finished", |record| {
        record["status"] == "Done"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "fay");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// The same, when the journal can't be read at the refusal: a read that
/// failed is not proof that no one moved the record. The decision waits for
/// the journal, which names the person.
#[tokio::test(flavor = "multi_thread")]
async fn a_journal_that_cannot_be_read_does_not_stop_the_timer() {
    let store = SimEventStore::no_faults(7);
    let state = server("unread", &store, 1).await;
    act(&state, "Check", "c-3", "Begin", &AgentContext::system()).await;
    act(&state, "Check", "c-3", "Nudge", &person("hal")).await;
    // The record's actor is live, so the stop reads only the journal.
    store.fail_next_reads("default:Check:c-3", 2);
    let record = until(&state, "Checks", "c-3", "finished", |record| {
        record["status"] == "Done"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "hal");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// A person's chain never stops: this is the same Poller, started by a person.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_a_person_started_is_not_stopped() {
    let store = SimEventStore::no_faults(7);
    let state = server("named", &store, 1).await;
    act(&state, "Poller", "p-2", "Begin", &person("eve")).await;
    let record = until(&state, "Pollers", "p-2", "got stuck", |record| {
        record["status"] == "Stuck"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "eve");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}
