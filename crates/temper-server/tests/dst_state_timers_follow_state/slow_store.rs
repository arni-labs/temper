//! Every fire retains its row and gate until the store decides its outcome.
//! Regression seeds 0..48 cover overdue boot fires and untouched steady tickers.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::{BoxedEventStore, StateTimerStore};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;

use super::check_seeds;
use super::process_clock::ProcessClock;
use super::slow_store_env::{Publications, SlowStore};
use super::specs::CSDL;
use super::world::SimTimerRows;

const GATE: usize = 8; // BOOT_FIRES_AT_ONCE
const TICKER: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting"]
initial = "Idle"
allow_indefinite_states = ["Idle"]
[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
[[action]]
name = "Expire"
from = ["Waiting"]
[[state_timeout]]
state = "Waiting"
after_seconds = 10
on_timeout = "Expire"
reset_on = ["Expire"]
"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

fn server(
    store: &SlowStore,
    rows: &Arc<SimTimerRows>,
    pubs: &Arc<Publications>,
    seed: u64,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "alpha",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", TICKER)],
    );
    let mut storage = StorageStack::from_sim(store.journal.clone(), None);
    storage.events = BoxedEventStore::new(store.clone());
    storage.query_plane = Some(pubs.clone());
    let mut state = ServerState::from_registry(ActorSystem::new(format!("slow-{seed}")), registry);
    state.set_storage_stack(storage);
    state.state_timer_rows = Some(rows.clone());
    state
}

#[test]
fn dst_slow_store_fire_keeps_timer_gate_and_publications() {
    check_seeds(run_seed);
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let store = SlowStore::new(seed);
    let rows = Arc::new(SimTimerRows::new(seed));
    let pubs = Arc::new(Publications::default());
    let mut rng = DeterministicRng::new(seed ^ 0xb007);
    let count = 2 * GATE + 1 + rng.next_bound(8);
    let tenant = TenantId::new("alpha");
    let first = runtime();
    first.block_on(async {
        clock.begin();
        let state = server(&store, &rows, &pubs, seed);
        state.start_runtime().await.unwrap().await.unwrap();
        for i in 0..count {
            let id = format!("job-{i}");
            state
                .get_or_create_tenant_entity(&tenant, "Job", &id, json!({}))
                .await
                .unwrap();
            assert!(
                state
                    .dispatch_tenant_action(
                        &tenant,
                        "Job",
                        &id,
                        "Start",
                        json!({}),
                        &AgentContext::default()
                    )
                    .await
                    .unwrap()
                    .success
            );
        }
        clock.end(Duration::from_secs(60));
    });
    drop(first);
    *store.faults.lock().unwrap() = true;
    let second = runtime();
    second.block_on(async {
        clock.begin();
        let state = server(&store, &rows, &pubs, seed);
        let mut events = state.event_tx.subscribe();
        state.start_runtime().await.unwrap();
        let mut broadcasts: BTreeMap<String, usize> = BTreeMap::new();
        let mut at_half = vec![0; count];
        let mut broken = BTreeMap::new();
        // Use small increments so every store completion, failure and timer
        // retry is observed without reads that could repair a missing timer.
        for step in 0..2400 {
            tokio::time::advance(Duration::from_millis(500)).await;
            // The only yielding I/O is a timer-row round trip (at most
            // seven turns). Drain the runnable work before inspecting it.
            for _ in 0..96 { tokio::task::yield_now().await; }
            while let Ok(event) = events.try_recv() {
                if event.action == "Expire" {
                    *broadcasts.entry(event.entity_id).or_default() += 1;
                }
            }
            let fail = |detail: String| format!("seed {seed}, step {step} ({}ms): {detail}", (step + 1) * 500);
            if store.activity.lock().unwrap().peak > GATE {
                broken.entry("gate").or_insert_with(|| fail(format!("fire writes exceed BOOT_FIRES_AT_ONCE: peak {}", store.activity.lock().unwrap().peak)));
            }
            let saved = rows.list_state_timers("alpha").await.unwrap();
            for (i, halfway) in at_half.iter_mut().enumerate() {
                let id = format!("job-{i}");
                let journal = store.journal.dump_journal(&format!("alpha:Job:{id}"));
                let fires: Vec<_> = journal.iter().filter(|e| e.payload["action"] == "Expire").collect();
                if step == 1199 { *halfway = fires.len(); }
                {
                    let timers: Vec<_> = saved.iter().filter(|row| row.entity_id == id).collect();
                    if timers.len() != 1 || timers[0].state != "Waiting" {
                        broken.entry("timer").or_insert_with(|| fail(format!("timed {id} needs exactly one armed Waiting timer, found {} ({} failed appends)", timers.len(), store.activity.lock().unwrap().failed)));
                    }
                }
                {
                    for event in &fires {
                        let seq = event.sequence_nr;
                        if broadcasts.get(&id).copied().unwrap_or(0) != fires.len() || !pubs.0.lock().unwrap().get(&id).is_some_and(|seqs| seqs.contains(&seq)) {
                            broken.entry("publication").or_insert_with(|| fail(format!("committed {id} at {seq} lacks broadcast or projection")));
                        }
                    }
                }
                if step == 2399 && (fires.len() <= *halfway || *halfway == 0) {
                    broken.entry("progress").or_insert_with(|| fail(format!("untouched {id} stopped ticking: {} fires at halfway, {} at end", halfway, fires.len())));
                }
            }
        }
        let a = store.activity.lock().unwrap();
        if a.failed == 0 || a.slow <= count {
            broken.entry("coverage").or_insert_with(|| format!("seed {seed}: no steady ticking: {} appends, {} failed", a.slow, a.failed));
        }
        if broken.is_empty() { Ok(()) } else { Err(broken.into_values().collect::<Vec<_>>().join("\n")) }
    })
}
