//! DST: a wake by someone other than the control plane leaves the reports
//! alone.
//!
//! Any caller Cedar permits may wake the process, the app's owner by hand
//! among them. Only the control plane reads a wake's answer as what it holds,
//! so only its wake may stand for a report delivered. While the control plane
//! cannot take reports, an owner's wake must not end the retry of the one on
//! its way, or the control plane never learns that due time and the sandbox
//! sleeps through it.
//!
//! Workload, per seed: the process boots and reports, then the control plane
//! fails every report (as a 503 does). The process arms a record due hours
//! away; its report fails and is retried. The owner wakes the process. Later
//! the control plane takes reports again.
//!
//! Invariant: once the control plane takes reports again, what it holds names
//! the record's due time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::wake::{SendError, WakeReport, WakeSink, Waker};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use super::world::{Clock, TimerRows};
use super::{CSDL, JOB, TENANT, act, settle};

/// Longer than the longest wait between two tries of a report (1 h).
const AFTER_RECOVERY: Duration = Duration::from_secs(3600 + 60);

/// A control plane that fails every report while `down`, and takes them
/// otherwise.
struct Outage {
    taken: Mutex<Vec<WakeReport>>,
    down: AtomicBool,
}

impl Outage {
    /// What the control plane holds: the newest report it took, by
    /// `(reported_at, seq)`.
    fn held(&self) -> Option<WakeReport> {
        self.taken
            .lock()
            .unwrap()
            .iter()
            .max_by_key(|report| (report.reported_at, report.seq))
            .cloned()
    }
}

#[async_trait::async_trait]
impl WakeSink for Outage {
    async fn send(&self, report: &WakeReport, _traceparent: Option<&str>) -> Result<(), SendError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(SendError::Undelivered("simulated 503".to_string()));
        }
        self.taken.lock().unwrap().push(report.clone());
        Ok(())
    }
}

/// The owner wakes the process by hand.
async fn owner_wakes(state: &ServerState) -> temper_server::wake::WakeOutcome {
    state
        .wake_runtime(&TenantId::new(TENANT), Waker::Other)
        .await
}

fn secs(rng: &mut DeterministicRng, from: u64, to: u64) -> Duration {
    Duration::from_secs(from + rng.next_bound((to - from) as usize) as u64)
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(Clock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let mut rng = DeterministicRng::new(seed);
    let events = SimEventStore::no_faults(seed);
    let outage = Arc::new(Outage {
        taken: Mutex::new(Vec::new()),
        down: AtomicBool::new(false),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        clock.begin();
        let mut registry = SpecRegistry::new();
        let csdl = parse_csdl(CSDL).unwrap();
        registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Job", JOB)]);
        let mut state = ServerState::from_registry(ActorSystem::new("manual"), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        state.state_timer_rows = Some(Arc::new(TimerRows::default()));
        state.set_wake_sink(outage.clone());
        state.start_runtime().await.expect("start").await.unwrap();
        settle().await;
        if outage.held().is_none() {
            return Err("the boot report is taken".to_string());
        }

        outage.down.store(true, Ordering::SeqCst);
        let id = format!("job-{seed}");
        let due: DateTime<Utc> = sim_now() + chrono::Duration::hours(10);
        state
            .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", &id, json!({}))
            .await
            .unwrap();
        if !act(&state, &id, "Plan", json!({ "due": due.to_rfc3339() })).await {
            return Err(format!("{id} Plan refused"));
        }
        settle().await;

        tokio::time::sleep(secs(&mut rng, 1, 600)).await;
        let outcome = owner_wakes(&state).await;
        settle().await;
        if outcome.fired != 0 || outcome.next_due_at != Some(due) {
            return Err(format!("the owner's wake answered {outcome:?}"));
        }

        tokio::time::sleep(secs(&mut rng, 1, 3600)).await;
        outage.down.store(false, Ordering::SeqCst);
        tokio::time::sleep(AFTER_RECOVERY).await;
        settle().await;
        let held = outage.held();
        if held.as_ref().and_then(|report| report.next_due_at) != Some(due) {
            return Err(format!(
                "once the control plane takes reports again, it holds the record's due \
                 time {due}; it holds {held:?}"
            ));
        }
        Ok(())
    });
    drop(runtime);
    result.map_err(|failure| format!("seed {seed}: {failure}"))
}

#[test]
fn dst_an_owners_wake_leaves_a_failing_report_to_be_retried() {
    let failures: Vec<String> = (0..32).filter_map(|seed| run_seed(seed).err()).collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
