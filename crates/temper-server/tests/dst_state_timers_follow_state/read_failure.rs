//! Regression for a transient install-walk journal failure.

use super::*;

/// A timeout added to a nonresident record must survive a transient journal
/// failure without a dispatch, restart, or pre-existing timer to rescue it.
#[test]
fn install_retries_unreadable_record_without_a_timer() {
    let seed = 96;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let untimed = job_untimed();
    let job = Job {
        tenant: TenantId::new("gamma"),
        id: "retry-read".to_string(),
    };
    let first = Process::start(&world, JOB_V1, &untimed, "read-failure-a");
    first.runtime.block_on(async {
        first
            .state
            .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
            .await
            .unwrap();
        assert!(act(&first.state, &job, "Start").await);
        settle().await;
        assert!(world.rows.row("gamma", &job.id).is_none());
    });
    first.kill(&world, Duration::ZERO);

    let second = Process::start(&world, JOB_V1, &untimed, "read-failure-b");
    second.runtime.block_on(async {
        // Index before injecting the fault: this must fail the record read,
        // not the independently retried listing of the type.
        second
            .state
            .ensure_type_indexed(&job.tenant, "Job")
            .await
            .unwrap();
        install(&second.state, "gamma", JOB_V1);
        world.events.fail_next_reads("gamma:Job:retry-read", 1);
        let state = second.state.clone();
        let follow = tokio::spawn(async move { state.follow_spec_changes().await });
        settle().await;
        assert!(world.rows.row("gamma", &job.id).is_none());
        assert!(
            !follow.is_finished(),
            "an unreadable record must remain pending"
        );
        advance(Duration::from_secs(1)).await;
        assert!(
            follow.is_finished(),
            "the retry must finish after storage recovers"
        );
        follow.await.unwrap();
        assert_eq!(world.rows.row("gamma", &job.id).unwrap().state, "Waiting");
        // No external dispatch after the failed read: the repaired timer alone
        // must move the record out of Waiting.
        advance(Duration::from_secs(3601)).await;
        assert_eq!(status(&second.state, &job).await, "Done");
    });
    second.kill(&world, Duration::ZERO);
}
