//! Saved service arms must yield to a caller when an install binds identity.
use super::*;
use temper_authz::{PrincipalKind, SecurityContext};

#[test]
fn saved_service_timer_waits_for_a_valid_caller_after_install_and_restart() {
    for seed in 0..8 {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let bound = JOB_V1.replace(
            "name = \"Expire\"",
            "name = \"Expire\"\nparams = [{ name = \"subject\", type = \"string\", source = \"authenticated_subject\" }]",
        );
        let first = Process::start(&world, JOB_V1, JOB_V1, "identity-before");
        let job = Job {
            tenant: TenantId::new("alpha"),
            id: "saved-service".into(),
        };
        first.runtime.block_on(async {
            first
                .state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .unwrap();
            let response = first
                .state
                .dispatch_tenant_action(
                    &job.tenant,
                    "Job",
                    &job.id,
                    "Start",
                    json!({}),
                    &AgentContext::for_service("state-timer"),
                )
                .await
                .unwrap();
            assert!(response.success);
            settle().await;
            assert!(world.rows.row("alpha", &job.id).is_some());
        });
        // Exercise both an in-process install and a saved row restored at boot.
        let process = if seed % 2 == 0 {
            first.runtime.block_on(async {
                install(&first.state, "alpha", &bound);
                first.state.follow_spec_changes().await;
                settle().await;
            });
            first
        } else {
            first.kill(&world, Duration::ZERO);
            Process::start(&world, &bound, JOB_V1, "identity-after")
        };
        process.runtime.block_on(async {
            settle().await;
            assert!(
                world.rows.row("alpha", &job.id).is_none(),
                "seed {seed}: unusable arm survives"
            );
            advance(Duration::from_secs(3601)).await;
            assert_eq!(status(&process.state, &job).await, "Waiting");
            assert!(
                process
                    .state
                    .dispatch_tenant_action(
                        &job.tenant,
                        "Job",
                        &job.id,
                        "Expire",
                        json!({}),
                        &AgentContext::for_service("state-timer"),
                    )
                    .await
                    .is_err(),
                "service must still be refused"
            );
            let mut security = SecurityContext::anonymous();
            security.principal.id = "alice".into();
            security.principal.kind = PrincipalKind::Customer;
            let caller = AgentContext {
                security_ctx: Some(security),
                ..AgentContext::default()
            };
            let response = process
                .state
                .dispatch_tenant_action(&job.tenant, "Job", &job.id, "Poke", json!({}), &caller)
                .await
                .unwrap();
            assert!(response.success);
            settle().await;
            advance(Duration::from_secs(1)).await;
            assert_eq!(status(&process.state, &job).await, "Done", "seed {seed}");
        });
        process.kill(&world, Duration::ZERO);
    }
}
