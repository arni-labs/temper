//! Boot recovery must wait for the install's policies and WASM modules.

use super::*;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_server::storage::StateTimerStore;

#[test]
fn dst_rowless_recovery_waits_for_module_loading() {
    recovery_waits_for_module_loading::<0>();
}

#[test]
fn dst_saved_row_recovery_waits_for_module_loading() {
    recovery_waits_for_module_loading::<1>();
}

#[test]
fn dst_dropped_row_recovery_waits_for_module_loading() {
    recovery_waits_for_module_loading::<2>();
}

fn recovery_waits_for_module_loading<const ROW: u8>() {
    check_seeds(|seed| {
        let saved_state = match ROW {
            0 => None,
            1 => Some("Waiting"),
            2 => Some("Idle"),
            _ => unreachable!(),
        };
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let world = World {
            events: SimEventStore::no_faults(seed),
            rows: Arc::new(SimTimerRows::new(seed)),
            clock,
        };
        let job = Job {
            tenant: TenantId::new("gamma"),
            id: "installing".into(),
        };
        let healthy = Job {
            tenant: TenantId::new("alpha"),
            id: "healthy".into(),
        };
        let first = Process::start(&world, JOB_V1, JOB_V1, "install-a");
        first.runtime.block_on(async {
            first
                .state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .unwrap();
            let caller = AgentContext {
                agent_id: Some("alice".into()),
                security_ctx: Some(SecurityContext::from_verified_jwt(
                    "alice",
                    PrincipalKind::Agent,
                    Some("test"),
                    None,
                    None,
                    None,
                )),
                ..AgentContext::default()
            };
            assert!(
                first
                    .state
                    .dispatch_tenant_action(
                        &job.tenant,
                        "Job",
                        &job.id,
                        "Start",
                        json!({}),
                        &caller
                    )
                    .await
                    .unwrap()
                    .success
            );
            first
                .state
                .get_or_create_tenant_entity(&healthy.tenant, "Job", &healthy.id, json!({}))
                .await
                .unwrap();
            assert!(act(&first.state, &healthy, "Start").await);
            settle().await;
        });
        let mut saved = world.rows.row("gamma", &job.id).unwrap();
        first.runtime.block_on(async {
            if let Some(state) = saved_state {
                saved.state = state.into();
                world.rows.upsert_state_timer(&saved).await.unwrap();
            } else {
                world
                    .rows
                    .delete_state_timer("gamma", "Job", &job.id, &saved.arm_id)
                    .await
                    .unwrap();
            }
        });
        first.kill(&world, Duration::from_secs(7200));
        world
            .rows
            .fail_next_listings("gamma", 1 + seed as usize % 3);
        // No timeouts at boot: the install introduces the first one while
        // recovery waits for its durable rows to become readable.
        let (second, walk) = Process::boot(&world, JOB_V1, &job_untimed(), "install-b");
        second.runtime.block_on(async {
            settle().await;
            assert!(!walk.is_finished());
            let hold = second.state.hold_timer_follow(&job.tenant);
            install(&second.state, "gamma", &module_spec(saved_state.is_some()));
            // The install has registered specs but has not loaded its module.
            second.state.follow_spec_changes().await;
            advance(Duration::from_secs(10 + seed)).await;
            assert_eq!(
                status(&second.state, &healthy).await,
                "Done",
                "another tenant's ready timers must keep firing"
            );
            assert_eq!(
                status(&second.state, &job).await,
                "Waiting",
                "an overdue timeout must not run before its module is loaded"
            );
            assert!(
                !walk.is_finished(),
                "recovery must retain responsibility during install"
            );
            settle().await;
            if saved_state.is_some() {
                assert_eq!(
                    world.rows.row("gamma", &job.id).unwrap().armed_by,
                    saved.armed_by,
                    "recovery must preserve the saved caller while the install waits"
                );
            } else {
                assert!(world.rows.row("gamma", &job.id).is_none());
            }
            // Load the real module and policies before releasing the install.
            let payload = r#"{"action":"Approve","params":{},"success":true}"#;
            let data = payload
                .bytes()
                .map(|byte| format!("\\{byte:02x}"))
                .collect::<String>();
            let wat = format!(
                r#"(module
                (import "env" "host_set_result" (func $result (param i32 i32)))
                (memory (export "memory") 1)
                (data (i32.const 0) "{data}")
                (func (export "run") (param i32 i32) (result i32)
                    i32.const 0 i32.const {} call $result i32.const 0))"#,
                payload.len()
            );
            let hash = second
                .state
                .wasm_engine
                .compile_and_cache(wat.as_bytes())
                .unwrap();
            second
                .state
                .register_wasm_module(&job.tenant, "finish", &hash);
            second
                .state
                .authz
                .reload_tenant_policies("gamma", "permit(principal, action, resource);")
                .unwrap();
            drop(hold);
            // Recovery must carry the caller through this handoff, even if
            // the install's own follow-up runs before recovery resumes.
            second.state.follow_spec_changes().await;
            let counts = walk.await.unwrap();
            // WASM runs on a real worker thread. Keep the test runnable so
            // simulated time cannot jump to Review's timer while that thread
            // finishes; the wall-clock bound only detects a stuck worker.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while matches!(
                status(&second.state, &job).await.as_str(),
                "Waiting" | "Review"
            ) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "WASM callback did not finish"
                );
                tokio::task::yield_now().await;
            }
            settle().await;
            assert_eq!(
                status(&second.state, &job).await,
                "Done",
                "the timer and real WASM success callback must finish the job"
            );
            let counted = counts[&(job.tenant.clone(), "Job".into())];
            assert_eq!(counted.waiting, 0);
            assert_eq!(counted.rearmed, usize::from(saved_state == Some("Waiting")));
            assert_eq!(counted.dropped, usize::from(saved_state == Some("Idle")));
            assert_eq!(counted.armed, usize::from(saved_state != Some("Waiting")));
            assert_eq!(counted.overdue, 1);
            let record = second
                .state
                .get_tenant_entity_state(&job.tenant, "Job", &job.id)
                .await
                .unwrap()
                .state;
            let fires = record
                .events
                .iter()
                .filter(|event| event.action == "Expire")
                .collect::<Vec<_>>();
            assert_eq!(fires.len(), 1);
            if saved_state.is_some() {
                assert_eq!(
                    fires[0].params["caller"],
                    json!("alice"),
                    "the timeout must fire with the durable caller's authenticated subject"
                );
            }
            assert!(!record.events.iter().any(|event| event.action == "Fail"));
            advance(Duration::from_secs(7200)).await;
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
                    .filter(|event| event.action == "Expire")
                    .count(),
                1
            );
        });
        second.kill(&world, Duration::ZERO);
        Ok(())
    });
}

fn module_spec(binds_caller: bool) -> String {
    let spec = JOB_V1.replace(
        "name = \"Expire\"\nfrom = [\"Waiting\", \"Review\", \"Done\"]\nto = \"Done\"",
        r#"name = "Expire"
from = ["Waiting"]
to = "Review"
[[action.triggers]]
name = "finish"
kind = "wasm"
module = "finish"
on_success = "Approve"
on_failure = "Fail"
[[action]]
name = "Fail"
from = ["Review"]
to = "Idle""#,
    );
    if binds_caller {
        spec.replace("initial = \"Idle\"", "initial = \"Idle\"\nstrict_action_params = true").replace("name = \"Expire\"", "name = \"Expire\"\nparams = [{ name = \"caller\", type = \"string\", source = \"authenticated_subject\" }]")
    } else {
        spec
    }
}
