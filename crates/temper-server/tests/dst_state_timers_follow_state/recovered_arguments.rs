//! Recovery follows installed timeout arguments while retaining the durable caller.

use super::*;

#[test]
fn dst_changed_timeout_during_row_retry_rebuilds_arguments() {
    check_seeds(|seed| {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let job = Job {
            tenant: TenantId::new("gamma"),
            id: "queued-caller".into(),
        };
        let old = JOB_V1.replacen(
            "on_timeout = \"Expire\"",
            "on_timeout = \"Expire\"\nargs = { marker = \"'old'\" }",
            1,
        );
        let new = JOB_V1.replacen(
            "on_timeout = \"Expire\"",
            "on_timeout = \"Approve\"\nargs = { marker = \"Id\" }",
            1,
        );
        let first = Process::start(&world, JOB_V1, &old, "queued-a");
        first.runtime.block_on(async {
            first
                .state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .unwrap();
            assert!(act(&first.state, &job, "Start").await);
            settle().await;
        });
        let saved = world.rows.row("gamma", &job.id).unwrap();
        first.kill(&world, Duration::ZERO);
        world
            .rows
            .fail_next_listings("gamma", 1 + seed as usize % 3);
        let (second, walk) = Process::boot(&world, JOB_V1, &old, "queued-b");
        second.runtime.block_on(async {
            settle().await;
            assert!(!walk.is_finished());
            install(&second.state, "gamma", &new);
            second.state.follow_spec_changes().await;
            let counts = walk.await.unwrap();
            settle().await;
            let recovered = world.rows.row("gamma", &job.id).expect("timer restored");
            assert_eq!(
                recovered.armed_by, saved.armed_by,
                "queued install must retain the saved identity"
            );
            let counted = counts[&(job.tenant.clone(), "Job".into())];
            assert_eq!(counted.rearmed, 1);
            assert_eq!(counted.dropped, 0);
            assert_eq!(counted.armed, 0);
            advance(Duration::from_secs(3601)).await;
            assert_eq!(status(&second.state, &job).await, "Done");
            let record = second
                .state
                .get_tenant_entity_state(&job.tenant, "Job", &job.id)
                .await
                .unwrap()
                .state;
            assert_eq!(
                record
                    .events
                    .iter()
                    .find(|event| event.action == "Approve")
                    .unwrap()
                    .params["marker"],
                json!(job.id)
            );
            assert_eq!(
                record
                    .events
                    .iter()
                    .filter(|event| event.action == "Approve")
                    .count(),
                1
            );
        });
        second.kill(&world, Duration::ZERO);
        Ok(())
    });
}
