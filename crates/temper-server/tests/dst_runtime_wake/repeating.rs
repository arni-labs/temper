//! A repeating state timeout arms its successor before its own fire ends.
use std::sync::Arc;

use async_trait::async_trait;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::storage::StateTimerStore;
use temper_server::wake::Waker;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tokio::sync::Notify;

use super::world::{Clock, TimerRows};
use super::{CSDL, JOB, TENANT, TIMEOUT, act, job, settle};

#[derive(Default)]
struct HeldDelete {
    rows: TimerRows,
    deleting: Notify,
    release: Notify,
}

#[async_trait]
impl StateTimerStore for HeldDelete {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.rows.upsert_state_timer(row).await
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        self.rows.list_state_timers(tenant).await
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        // Hold the old fire after dispatch has armed its successor.
        self.deleting.notify_one();
        self.release.notified().await;
        self.rows
            .delete_state_timer(tenant, entity_type, entity_id, arm_id)
            .await
    }
}

#[test]
fn dst_repeating_state_timeout_wake_waits_for_its_own_completion() {
    for seed in 0..8 {
        let clock = Arc::new(Clock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async {
            clock.begin();
            let spec = JOB.replace(
                "to = \"Done\"\n\n[[action]]\nname = \"Activate\"",
                "to = \"Waiting\"\n\n[[action]]\nname = \"Activate\"",
            ) + "\nreset_on = [\"Expire\"]\n";
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                TENANT,
                parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[("Job", &spec)],
            );
            let rows = Arc::new(HeldDelete::default());
            let mut state = ServerState::from_registry(ActorSystem::new("repeating"), registry);
            state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
            state.state_timer_rows = Some(rows.clone());
            state.start_runtime().await.unwrap().await.unwrap();
            job(&state, "repeating").await;
            assert!(act(&state, "repeating", "Wait", serde_json::json!({})).await);
            settle().await;
            clock.suspend(std::time::Duration::from_secs(TIMEOUT as u64 + seed + 1));
            let wake_state = state.clone();
            let wake = tokio::spawn(async move {
                wake_state
                    .wake_runtime(&TenantId::new(TENANT), Waker::ControlPlane)
                    .await
            });
            rows.deleting.notified().await;
            settle().await;
            assert!(
                !wake.is_finished(),
                "seed {seed}: wake answered before fire ended"
            );
            let successor = sim_now() + chrono::Duration::seconds(TIMEOUT);
            assert_eq!(state.next_due_at(), Some(successor));
            // A second wake arriving after replacement must also wait for the old fire.
            let wake_state = state.clone();
            let concurrent = tokio::spawn(async move {
                wake_state
                    .wake_runtime(&TenantId::new(TENANT), Waker::ControlPlane)
                    .await
            });
            settle().await;
            assert!(
                !concurrent.is_finished(),
                "seed {seed}: concurrent wake lost the firing registration"
            );
            rows.release.notify_one();
            for wake in [wake, concurrent] {
                let outcome = wake.await.unwrap();
                assert_eq!(outcome.fired, 1, "seed {seed}: {outcome:?}");
                assert_eq!(outcome.next_due_at, Some(successor));
            }
            assert_eq!(
                state
                    .wake_runtime(&TenantId::new(TENANT), Waker::ControlPlane)
                    .await
                    .fired,
                0
            );
        });
    }
}
