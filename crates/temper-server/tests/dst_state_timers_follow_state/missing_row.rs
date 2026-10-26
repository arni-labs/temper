//! Missing-row recovery preserves the state clock and caller requirement.
use super::*;

#[test]
fn dst_rehydrate_arms_a_missing_row_for_only_the_remaining_budget() {
    check_seeds(|seed| {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let process = Process::start(&world, JOB_V1, &job_untimed(), "missing-row");
        process.runtime.block_on(async {
            let job = Job {
                tenant: TenantId::new("gamma"),
                id: "legacy".into(),
            };
            process
                .state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .unwrap();
            assert!(act(&process.state, &job, "Start").await);
            advance(Duration::from_secs(301)).await;
            while process.state.passivate_idle_actors().await > 0 {}
            install(&process.state, "gamma", JOB_V1);
            // Isolate rehydration: no install walk may rescue this record.
            process.state.registry.write().unwrap().take_changed_types();
            assert!(world.rows.row("gamma", &job.id).is_none());
            assert_eq!(status(&process.state, &job).await, "Waiting");
            settle().await;
            let row = world.rows.row("gamma", &job.id).unwrap();
            let identity: serde_json::Value = serde_json::from_str(&row.armed_by).unwrap();
            assert_eq!(identity["agent_id"], "service:state-timer");
            advance(Duration::from_secs(3298)).await;
            assert_eq!(status(&process.state, &job).await, "Waiting");
            advance(Duration::from_secs(2)).await;
            assert_eq!(status(&process.state, &job).await, "Done");
        });
        process.kill(&world, Duration::ZERO);
        Ok(())
    });
}

#[test]
fn missing_row_with_authenticated_subject_waits_for_a_caller() {
    let seed = 147;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let first = Process::start(&world, JOB_V1, &job_untimed(), "caller-a");
    let job = Job {
        tenant: TenantId::new("gamma"),
        id: "needs-caller".into(),
    };
    first.runtime.block_on(async {
        first
            .state
            .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
            .await
            .unwrap();
        assert!(act(&first.state, &job, "Start").await);
    });
    first.kill(&world, Duration::from_secs(7200));
    let bound = JOB_V1.replace("initial = \"Idle\"", "initial = \"Idle\"\nstrict_action_params = true").replace("name = \"Expire\"", "name = \"Expire\"\nparams = [{ name = \"caller\", type = \"string\", source = \"authenticated_subject\" }]");
    let second = Process::start(&world, JOB_V1, &bound, "caller-b");
    assert_eq!(
        second.booted[&(job.tenant.clone(), "Job".into())].waiting,
        1
    );
    second.runtime.block_on(async {
        advance(Duration::from_secs(7200)).await;
        assert_eq!(status(&second.state, &job).await, "Waiting");
        assert!(world.rows.row("gamma", &job.id).is_none());
    });
    second.kill(&world, Duration::ZERO);
}
