//! A process exit after the event commits must not leave a cold record stale.
use super::*;
use crate::storage::{BackendLabel, BoxedEventStore, StateTimerStore};
use std::collections::BTreeMap;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_store_sim::SimEventStore;

#[path = "../../../../../tests/dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
use process_clock::ProcessClock;

const TENANT: &str = "installed-factory";
const IOA: &str = r#"
[automaton]
name = "Order"
states = ["Reviewing", "Checked", "Done"]
initial = "Reviewing"
allow_indefinite_states = ["Reviewing", "Done"]
[[action]]
name = "Review"
from = ["Reviewing"]
to = "Checked"
[[action]]
name = "Finish"
from = ["Checked"]
to = "Done"
[[state_timeout]]
state = "Checked"
after_seconds = 2100
on_timeout = "Finish"
"#;

#[derive(Default)]
struct Rows {
    rows: Mutex<BTreeMap<String, StateTimerRecord>>,
    // Exit before/after the timer row write, as well as before the catalog write.
    cut: AtomicUsize,
    hanging: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl StateTimerStore for Rows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        if self.cut.load(Ordering::SeqCst) == 1 {
            self.hanging.notify_one();
            std::future::pending::<()>().await;
        }
        self.rows
            .lock()
            .unwrap()
            .insert(row.entity_id.clone(), row.clone());
        if self.cut.load(Ordering::SeqCst) == 2 {
            self.hanging.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn delete_state_timer(
        &self,
        _tenant: &str,
        _ty: &str,
        id: &str,
        arm: &str,
    ) -> Result<(), PersistenceError> {
        let mut rows = self.rows.lock().unwrap();
        if rows.get(id).is_some_and(|r| r.arm_id == arm) {
            rows.remove(id);
        }
        Ok(())
    }
    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self
            .rows
            .lock()
            .unwrap()
            .values()
            .filter(|r| r.tenant == tenant)
            .cloned()
            .collect())
    }
}

fn server(events: &SimEventStore, qp: Arc<FlakyQueryPlane>, rows: Arc<Rows>) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("boot-repair"), registry);
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(events.clone()),
        None,
        None,
        None,
        None,
        Some(qp),
        None,
        None,
        None,
    ));
    state.state_timer_rows = Some(rows);
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
    state
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

fn restart(seed: u64, cut: usize) {
    let clock = Arc::new(ProcessClock::new());
    let _context = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let events = SimEventStore::no_faults(seed);
    let qp = Arc::new(FlakyQueryPlane {
        fail_write: usize::MAX,
        ..Default::default()
    });
    let rows = Arc::new(Rows::default());
    let tenant = TenantId::new(TENANT);
    let first = runtime();
    first.block_on(async {
        clock.begin();
        let state = server(&events, qp.clone(), rows.clone());
        state.get_or_create_tenant_entity(&tenant, "Order", "release", serde_json::json!({})).await.unwrap();
        if cut == 0 { qp.hang_sequence.store(2, Ordering::SeqCst); }
        else { rows.cut.store(cut, Ordering::SeqCst); }
        let ctx = AgentContext::for_service("restart-regression");
        let dispatch = state.dispatch_tenant_action(&tenant, "Order", "release", "Review", serde_json::json!({}), &ctx);
        tokio::pin!(dispatch);
        tokio::select! {
            result = &mut dispatch => panic!("dispatch escaped cut {cut}: {result:?}"),
            _ = async { if cut == 0 { qp.hanging.notified().await } else { rows.hanging.notified().await } } => {}
        }
        // Dropping the dispatch and runtime is a process exit; the event is durable.
        let journal = state.event_journal().unwrap().0.read_events(&format!("{TENANT}:Order:release"), 0).await.unwrap();
        assert_eq!(journal.last().unwrap().payload["to_status"], "Checked");
        clock.end(Duration::from_secs(600));
    });
    drop(first);
    qp.hang_sequence.store(0, Ordering::SeqCst);
    rows.cut.store(0, Ordering::SeqCst);
    let second = runtime();
    second.block_on(async {
        clock.begin();
        let state = server(&events, qp.clone(), rows.clone());
        state.start_runtime().await.unwrap().await.unwrap();
        assert!(
            state
                .resident_entity_sequence(&tenant, "Order", "release")
                .await
                .is_none()
        );
        let body = crate::odata::read::load_existing_entity_descriptor_body(
            &state, &tenant, "Order", "Orders", "release",
        )
        .await
        .unwrap();
        assert_eq!(
            body["status"], "Checked",
            "seed {seed}, cut {cut}: cold point read must match journal"
        );
        assert_eq!(
            state.state_timeout_tracker.pending_snapshot(),
            vec![("Order".to_string(), 1)],
            "seed {seed}, cut {cut}: exactly one armed timer"
        );
        assert_eq!(rows.list_state_timers(TENANT).await.unwrap().len(), 1);
        // Ten minutes were spent down: only 25 of the 35 minutes remain.
        tokio::time::sleep(Duration::from_secs(1499)).await;
        let before = state
            .get_tenant_entity_state(&tenant, "Order", "release")
            .await
            .unwrap();
        assert_eq!(before.state.status, "Checked");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let after = state
            .get_tenant_entity_state(&tenant, "Order", "release")
            .await
            .unwrap();
        assert_eq!(
            after.state.status, "Done",
            "seed {seed}, cut {cut}: repaired timer fires on original deadline"
        );
        assert_eq!(after.state.sequence_nr, 3, "timeout fires exactly once");
    });
}

#[test]
fn a_cold_installed_app_record_and_timer_recover_after_exit() {
    restart(0, 0);
}

#[test]
fn dst_exits_between_event_commit_and_last_row_write() {
    let mut failures = Vec::new();
    for seed in 0..64 {
        let cut = DeterministicRng::new(seed).next_bound(3);
        if std::panic::catch_unwind(|| restart(seed, cut)).is_err() {
            failures.push((seed, cut));
        }
    }
    assert!(
        failures.is_empty(),
        "restart failures (seed, cut): {failures:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cold_point_read_checks_the_journal_even_without_boot_repair() {
    let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(91);
    let events = SimEventStore::no_faults(91);
    let qp = Arc::new(FlakyQueryPlane {
        fail_write: usize::MAX,
        ..Default::default()
    });
    let rows = Arc::new(Rows::default());
    let tenant = TenantId::new(TENANT);
    let state = server(&events, qp.clone(), rows.clone());
    let result = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "cold",
            "Review",
            serde_json::json!({}),
            &AgentContext::for_service("cold-read"),
        )
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    qp.inner
        .upsert_projection(
            TENANT,
            "Order",
            "cold",
            "Reviewing",
            &serde_json::json!({}),
            &serde_json::json!({"status": "Reviewing"}),
            result.state.sequence_nr - 1,
        )
        .await
        .unwrap();
    let second = server(&events, qp, rows);
    assert!(
        try_load_entity_body_from_catalog(&second, &tenant, "Order", "Orders", "cold", true)
            .await
            .is_none(),
        "a cold read must reject a row behind its journal"
    );
    assert!(
        second
            .resident_entity_sequence(&tenant, "Order", "cold")
            .await
            .is_none(),
        "the check itself must not load an actor"
    );
}
