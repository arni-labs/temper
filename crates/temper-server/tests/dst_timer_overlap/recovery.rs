//! Crash at the durable handoff of a stale timer, then boot from its row.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_server::storage::StateTimerStore;

struct HandoffRows {
    inner: SimTimerRows,
    pause: AtomicBool,
    reached: AtomicBool,
}

impl HandoffRows {
    async fn boundary(&self) {
        if self.pause.load(Ordering::SeqCst) {
            self.reached.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait::async_trait]
impl StateTimerStore for HandoffRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        // Stop before the successor is durable.
        self.boundary().await;
        self.inner.upsert_state_timer(row).await
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.inner
            .delete_state_timer(tenant, entity_type, entity_id, arm_id)
            .await?;
        // On the broken implementation, stop after deleting the only row.
        self.boundary().await;
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        self.inner.list_state_timers(tenant).await
    }
}

#[test]
fn dst_restart_during_stale_timer_handoff() {
    for seed in 0..16 {
        for remote_move in [false, true] {
            let clock = Arc::new(ProcessClock::new());
            let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
            let events = SimEventStore::no_faults(seed);
            let rows = Arc::new(HandoffRows {
                inner: SimTimerRows::new(seed),
                pause: AtomicBool::new(false),
                reached: AtomicBool::new(false),
            });
            let fires = Arc::new(Fires::default());
            let tenant = TenantId::new(TENANT);
            let runtime = || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .start_paused(true)
                    .build()
                    .unwrap()
            };
            let process = runtime();
            process.block_on(async {
                clock.begin();
                let first = server(&events, &rows, &fires, "before-crash").await;
                first
                    .get_or_create_tenant_entity(&tenant, "Job", "a", json!({}))
                    .await
                    .unwrap();
                assert!(
                    first
                        .dispatch_tenant_action(
                            &tenant,
                            "Job",
                            "a",
                            "Start",
                            json!({}),
                            &AgentContext::default(),
                        )
                        .await
                        .unwrap()
                        .success
                );
                settle().await;
                let original = rows.inner.row(TENANT, "a").unwrap();
                let writer = if remote_move {
                    server(&events, &rows, &fires, "other-writer").await
                } else {
                    first.clone()
                };
                assert!(
                    writer
                        .dispatch_tenant_action(
                            &tenant,
                            "Job",
                            "a",
                            "Touch",
                            json!({}),
                            &AgentContext::default(),
                        )
                        .await
                        .unwrap()
                        .success
                );
                settle().await;
                assert_eq!(rows.inner.row(TENANT, "a").unwrap().arm_id, original.arm_id);
                // Local Touch exercises the pre-dispatch stale check. Remote
                // Touch leaves the first actor stale and exercises replay.
                rows.pause.store(true, Ordering::SeqCst);
                tokio::time::advance(Duration::from_secs(60)).await;
                settle().await;
                assert!(
                    rows.reached.load(Ordering::SeqCst),
                    "seed {seed}: handoff reached"
                );
                assert_eq!(fires.0.lock().unwrap().get("a").copied().unwrap_or(0), 0);
                clock.end(Duration::ZERO);
            });
            // Dropping the runtime aborts every timer task at the boundary.
            drop(process);
            assert!(
                rows.inner.row(TENANT, "a").is_some(),
                "seed {seed}, remote {remote_move}: crash must retain a recovery row",
            );
            rows.pause.store(false, Ordering::SeqCst);
            runtime().block_on(async {
                clock.begin();
                let _restarted = server(&events, &rows, &fires, "after-crash").await;
                settle().await;
                assert_eq!(
                    events
                        .dump_journal(&format!("{TENANT}:Job:a"))
                        .iter()
                        .filter(|e| e.event_type == "Check")
                        .count(),
                    1,
                    "seed {seed}, remote {remote_move}: boot recovers one due fire",
                );
                assert_eq!(fires.0.lock().unwrap()["a"], 1);
                assert!(
                    rows.inner.row(TENANT, "a").is_some(),
                    "successor survives cleanup"
                );
                clock.end(Duration::ZERO);
            });
        }
    }
}
