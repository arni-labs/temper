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
/// contention, arms the timer of the state it entered at once and starts its
/// dependents once, after the projection lands. It used to answer with an
/// error and leave the record in its new state with no timer and no follow-on
/// work, waiting for a retry that a timer, a reaction or a caller that gave up
/// never sends (a Factory release sat in Making that way).
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
        let mut events = state.event_tx.subscribe();
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
        let making = seen
            .iter()
            .filter(|(kind, status)| kind == "Source" && status == "Making")
            .count();
        assert_eq!(making, 1, "seed {seed}: one broadcast of the move");
    }
}

/// Read state changes until one reaches a status in `statuses`, keeping each
/// `(entity_type, status)` read in `seen`.
async fn until_status(
    events: &mut tokio::sync::broadcast::Receiver<crate::events::EntityStateChange>,
    statuses: &[&str],
    seen: &mut Vec<(String, String)>,
) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            seen.push((event.entity_type.clone(), event.status.clone()));
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

#[tokio::test]
async fn causal_projection_timeout_keeps_commit_and_stops_dependents() {
    let (state, query, _queue, _temp) = fixture().await;
    query.hang_source_write.store(true, Ordering::SeqCst);
    let tenant = TenantId::default();
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
    let expire = async {
        query.projection_started.notified().await;
        // Pause only after real journal I/O and actor processing have finished.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let (response, ()) = tokio::join!(submit, expire);
        response
    })
    .await;
    tokio::time::resume();
    let response = result.expect("projection barrier must be bounded").unwrap();
    assert!(!response.success);
    let error = response.error.as_deref().unwrap();
    assert!(
        error.contains("committed") && error.contains("timed out"),
        "{error}"
    );
    let source = state
        .get_tenant_entity_state(&tenant, "Source", "source")
        .await
        .unwrap();
    assert_eq!(source.state.fields["prompt_template"], "new");
    let child = state
        .get_tenant_entity_state(&tenant, "Verifier", "verifier")
        .await
        .unwrap();
    assert_eq!(child.state.status, "Idle");
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
        tokio::time::advance(std::time::Duration::from_secs(15)).await;
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let (response, ()) = tokio::join!(reset, observe);
        response
    })
    .await;
    tokio::time::resume();
    let response = result.unwrap().unwrap();
    assert!(!response.success);
    assert!(response.error.unwrap().contains("committed"));
}
