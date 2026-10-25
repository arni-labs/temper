//! Causal reads under projection lag, injected failures and idempotent recovery.
use super::*;
use crate::state::DispatchExtOptions;
use crate::storage::QueryPlaneStore;
use serde_json::json;
use std::sync::atomic::Ordering;
use temper_runtime::scheduler::install_deterministic_context;

#[path = "causal_projection_test/fixture_test.rs"]
mod fixture_test;
use fixture_test::fixture;

#[tokio::test]
async fn causal_submit_reaction_wasm_reads_committed_source_under_projection_lag() {
    for seed in 0..8 {
        let (_guard, _, _) = install_deterministic_context(seed);
        let (state, query, queue, _temp) = fixture().await;
        let params = if seed % 2 == 0 {
            json!({"prompt_template":"new"})
        } else {
            let edit = state
                .dispatch_tenant_action(
                    &TenantId::default(),
                    "Source",
                    "source",
                    "Edit",
                    json!({"prompt_template":"new"}),
                    &AgentContext::system(),
                )
                .await
                .unwrap();
            assert!(edit.success);
            // A retry-style transition changes no source fields, but its
            // declared reader still requires the previously committed edit.
            json!({})
        };
        let mut events = state.event_tx.subscribe();
        let response = state
            .dispatch_tenant_action_ext(
                &TenantId::default(),
                "Source",
                "source",
                "Submit",
                params,
                DispatchExtOptions {
                    agent_ctx: &AgentContext::system(),
                    await_integration: true,
                    await_reactions: true,
                },
            )
            .await
            .unwrap();
        assert!(response.success, "{response:?}");
        let observed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.entity_type == "Verifier"
                    && matches!(event.status.as_str(), "SawNew" | "SawOld")
                {
                    break event.status;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            observed, "SawNew",
            "seed {seed}: verifier observed stale source"
        );
        // A delayed older queued write cannot replace the visible commit.
        queue.enqueue_upsert(
            "default".into(),
            "Source".into(),
            "source".into(),
            "Draft".into(),
            json!({"prompt_template":"old"}),
            json!({}),
            0,
            "test_stale",
        );
        queue.drain_once_for_test().await;
        let rows = query
            .load_entity_catalog_rows("default", "Source", &["source".into()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rows[0].fields["prompt_template"], "new");
    }
}

/// A move whose projection write fails after its commit, as under Turso write
/// contention, arms the timer of the state it entered and its scheduled
/// action at once, and starts its dependents once, after the projection lands.
/// It used to answer with an error and leave the record in its new state with
/// no timer and no follow-on work, waiting for a retry that a timer, a
/// reaction or a caller that gave up never sends (a Factory release sat in
/// Making that way).
#[tokio::test]
async fn failed_projection_after_commit_arms_the_timer_and_starts_dependents_once() {
    for seed in 0..4 {
        let (_guard, _, _) = install_deterministic_context(seed);
        let (state, query, _queue, _temp) = fixture_test::fixture_entering_timed_state().await;
        let tenant = TenantId::default();
        let verifier_before = state
            .get_tenant_entity_state(&tenant, "Verifier", "verifier")
            .await
            .unwrap()
            .state
            .sequence_nr;
        let source_before = state
            .get_tenant_entity_state(&tenant, "Source", "source")
            .await
            .unwrap()
            .state
            .sequence_nr;
        let mut events = state.event_tx.subscribe();
        let mut pings = state.event_tx.subscribe();
        // Only the Submit's write is refused: the Ping it schedules lands.
        query
            .refused_sequence
            .store(source_before + 1, Ordering::SeqCst);
        query.fail_source_write.store(true, Ordering::SeqCst);
        let agent = AgentContext::system();
        let submit = state.dispatch_tenant_action_ext(
            &tenant,
            "Source",
            "source",
            "Submit",
            json!({"prompt_template":"new"}),
            DispatchExtOptions {
                agent_ctx: &agent,
                await_integration: true,
                await_reactions: true,
            },
        );
        let outage = async {
            query.source_write_refused.notified().await;
            let source = state
                .get_tenant_entity_state(&tenant, "Source", "source")
                .await
                .unwrap();
            assert_eq!(source.state.status, "Making", "seed {seed}: committed");
            assert!(
                state.has_armed_state_timer(&tenant, "Source", "source"),
                "seed {seed}: the Making timer is armed while the projection fails"
            );
            // The scheduled Ping fires while the Submit's projection fails,
            // and its timer is gone once it has.
            loop {
                let event = pings.recv().await.unwrap();
                if event.action == "Ping" {
                    break;
                }
            }
            while !state.armed_scheduled_actions().is_empty() {
                tokio::task::yield_now().await;
            }
            // Refused again: the write is tried again, and nothing it starts
            // runs before the source is visible.
            query.source_write_refused.notified().await;
            let verifier = state
                .get_tenant_entity_state(&tenant, "Verifier", "verifier")
                .await
                .unwrap();
            assert_eq!(
                verifier.state.sequence_nr, verifier_before,
                "seed {seed}: a dependent started before the projection landed"
            );
            let arms = state
                .state_timeout_tracker
                .sequence_for_test(&tenant, "Source", "source");
            query.fail_source_write.store(false, Ordering::SeqCst);
            arms
        };
        let (response, arms_during_outage) =
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                tokio::join!(submit, outage)
            })
            .await
            .expect("the dispatch finishes once the projection lands");
        let response = response.unwrap();
        assert!(response.success, "seed {seed}: {response:?}");
        assert_eq!(response.state.status, "Making");
        assert_eq!(
            state
                .state_timeout_tracker
                .sequence_for_test(&tenant, "Source", "source"),
            arms_during_outage,
            "seed {seed}: the timer is armed once, so its deadline counts from the commit"
        );

        // The Verifier's WASM trigger reads the source through OData and
        // answers SawNew only if it sees the committed change.
        let mut seen = Vec::new();
        let verified = until_status(&mut events, &["SawNew", "SawOld"], &mut seen).await;
        assert_eq!(verified, "SawNew", "seed {seed}");

        // The timer fires at its deadline.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(4500)).await;
        tokio::time::resume();
        assert_eq!(
            until_status(&mut events, &["Lost"], &mut seen).await,
            "Lost"
        );

        let verifier = state
            .get_tenant_entity_state(&tenant, "Verifier", "verifier")
            .await
            .unwrap();
        assert_eq!(verifier.state.status, "SawNew");
        assert_eq!(
            verifier.state.sequence_nr,
            verifier_before + 2,
            "seed {seed}: one Verify and one SawNew, so the reaction and the WASM trigger ran once"
        );
        let broadcasts = |action: &str| {
            seen.iter()
                .filter(|(kind, done, _)| kind == "Source" && done == action)
                .count()
        };
        assert_eq!(
            broadcasts("Submit"),
            1,
            "seed {seed}: one broadcast of the move"
        );
        assert_eq!(
            broadcasts("Ping"),
            1,
            "seed {seed}: the scheduled Ping fires once, not again when the move finishes"
        );
    }
}

/// Read state changes until one reaches a status in `statuses`, keeping each
/// `(entity_type, action, status)` read in `seen`.
async fn until_status(
    events: &mut tokio::sync::broadcast::Receiver<crate::events::EntityStateChange>,
    statuses: &[&str],
    seen: &mut Vec<(String, String, String)>,
) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            seen.push((
                event.entity_type.clone(),
                event.action.clone(),
                event.status.clone(),
            ));
            if statuses.contains(&event.status.as_str()) {
                break event.status;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no state change reached {statuses:?}; saw {seen:?}"))
}

/// ADR-0148 amendment: an ordinary edit is projected before its response, so a
/// caller that writes and then queries sees the write without waiting for the
/// queue.
#[tokio::test]
async fn ordinary_edits_are_projected_before_the_response() {
    let (state, query, _queue, _temp) = fixture().await;
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Source",
            "source",
            "Edit",
            json!({"prompt_template":"edited"}),
            &AgentContext::system(),
        )
        .await
        .unwrap();
    assert!(response.success);
    let rows = query
        .load_entity_catalog_rows("default", "Source", &["source".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rows[0].fields["prompt_template"], "edited",
        "the edit is visible to queries when dispatch returns"
    );
}

#[tokio::test]
async fn causal_delete_keeps_older_queued_writes_before_removal() {
    let (state, query, queue, _temp) = fixture().await;
    let mut events = state.event_tx.subscribe();
    let edited = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Source",
            "source",
            "Edit",
            json!({"prompt_template":"older queued"}),
            &AgentContext::system(),
        )
        .await
        .unwrap();
    assert!(edited.success);
    let deleted = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "Source",
            "source",
            "Delete",
            json!({"prompt_template":"new"}),
            DispatchExtOptions {
                agent_ctx: &AgentContext::system(),
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .unwrap();
    assert!(deleted.success, "{deleted:?}");
    let observed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.entity_type == "Verifier"
                && matches!(event.status.as_str(), "SawNew" | "SawOld")
            {
                break event.status;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(observed, "SawNew");
    queue.drain_once_for_test().await;
    let rows = query
        .load_entity_catalog_rows("default", "Source", &["source".into()])
        .await
        .unwrap()
        .unwrap();
    assert!(
        rows.is_empty(),
        "older queued write must not resurrect deleted projection"
    );
}

/// A delete committed while an older move's projection write keeps failing
/// ends that move's tries. The delete removes the record's row, so the older
/// write landing after the removal would bring the deleted record back.
#[tokio::test]
#[ignore = "shown red in run 40188752404"]
async fn a_delete_during_a_failed_projection_is_not_undone_by_its_retry() {
    let (state, query, queue, _temp) = fixture().await;
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    query.fail_source_write.store(true, Ordering::SeqCst);
    let edit = state.dispatch_tenant_action(
        &tenant,
        "Source",
        "source",
        "Edit",
        json!({"prompt_template":"new"}),
        &agent,
    );
    let delete = async {
        query.source_write_refused.notified().await;
        // The delete's own write carries other fields, so it lands, and its
        // queued removal takes the row away.
        let deleted = state
            .dispatch_tenant_action(
                &tenant,
                "Source",
                "source",
                "Delete",
                json!({"prompt_template":"gone"}),
                &agent,
            )
            .await
            .unwrap();
        assert!(deleted.success, "{deleted:?}");
        queue.drain_once_for_test().await;
        let rows = query
            .load_entity_catalog_rows("default", "Source", &["source".into()])
            .await
            .unwrap()
            .unwrap();
        assert!(rows.is_empty(), "the delete removed the row");
        // The store recovers while the edit is still being tried.
        query.fail_source_write.store(false, Ordering::SeqCst);
    };
    let (edited, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(edit, delete)
    })
    .await
    .expect("the edit finishes once the record is deleted");
    let edited = edited.unwrap();
    assert!(edited.success, "{edited:?}");
    let rows = query
        .load_entity_catalog_rows("default", "Source", &["source".into()])
        .await
        .unwrap()
        .unwrap();
    assert!(
        rows.is_empty(),
        "the edit's retried write brought the deleted record back: {rows:?}"
    );
}

/// A projection write that hangs past its budget is abandoned and tried
/// again, and the dependents start once the retry lands.
#[tokio::test]
async fn causal_projection_timeout_is_tried_again_before_dependents_start() {
    let (state, query, _queue, _temp) = fixture().await;
    query.hang_source_write.store(true, Ordering::SeqCst);
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let mut events = state.event_tx.subscribe();
    let submit = state.dispatch_tenant_action_ext(
        &tenant,
        "Source",
        "source",
        "Submit",
        json!({"prompt_template":"new"}),
        DispatchExtOptions {
            agent_ctx: &agent,
            await_integration: true,
            await_reactions: true,
        },
    );
    let expire = async {
        query.projection_started.notified().await;
        let child = state
            .get_tenant_entity_state(&tenant, "Verifier", "verifier")
            .await
            .unwrap();
        assert_eq!(child.state.status, "Idle", "started before the projection");
        // The next try does not hang.
        query.hang_source_write.store(false, Ordering::SeqCst);
        // Pause only after real journal I/O and actor processing have
        // finished, and resume before the retry's real I/O.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        tokio::time::resume();
    };
    let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(submit, expire)
    })
    .await
    .expect("the retried projection lands");
    let response = response.unwrap();
    assert!(response.success, "{response:?}");
    let source = state
        .get_tenant_entity_state(&tenant, "Source", "source")
        .await
        .unwrap();
    assert_eq!(source.state.fields["prompt_template"], "new");
    let mut seen = Vec::new();
    let verified = until_status(&mut events, &["SawNew", "SawOld"], &mut seen).await;
    assert_eq!(verified, "SawNew");
}

#[tokio::test]
async fn causal_reset_invalidates_old_timer_before_projection_wait() {
    let (state, query, _queue, _temp) = fixture_test::fixture_with_timeout().await;
    let tenant = TenantId::default();
    let agent = AgentContext::system();
    let first = state
        .dispatch_tenant_action(
            &tenant,
            "Source",
            "source",
            "Edit",
            json!({"prompt_template":"old"}),
            &agent,
        )
        .await
        .unwrap();
    assert!(first.success);
    let old_sequence = state
        .state_timeout_tracker
        .sequence_for_test(&tenant, "Source", "source");
    assert!(old_sequence > 0);
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(5)).await;
    tokio::time::resume();
    query.hang_source_write.store(true, Ordering::SeqCst);
    let reset = state.dispatch_tenant_action(
        &tenant,
        "Source",
        "source",
        "Edit",
        json!({"prompt_template":"new"}),
        &agent,
    );
    let observe = async {
        query.projection_started.notified().await;
        assert!(
            state
                .state_timeout_tracker
                .sequence_for_test(&tenant, "Source", "source")
                > old_sequence,
            "old timer remains valid after the reset transition committed"
        );
        assert!(
            state.has_armed_state_timer(&tenant, "Source", "source"),
            "the reset's own timer is armed before the projection lands"
        );
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(16)).await;
        tokio::task::yield_now().await;
        let source = state
            .get_tenant_entity_state(&tenant, "Source", "source")
            .await
            .unwrap();
        assert_eq!(
            source.state.status, "Draft",
            "previous deadline must not expire the reset state"
        );
        // The hung try times out; the next one lands, after real time resumes.
        query.hang_source_write.store(false, Ordering::SeqCst);
        tokio::time::advance(std::time::Duration::from_secs(15)).await;
        tokio::time::resume();
    };
    let (response, ()) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(reset, observe)
    })
    .await
    .expect("the retried projection lands");
    let response = response.unwrap();
    assert!(response.success, "{response:?}");
}
