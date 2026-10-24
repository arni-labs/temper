//! DST: a scheduled action outlives a restart and fires once.
//!
//! Workload, per seed: records arm `schedule(...)` and `schedule_at(...)`
//! actions, once (`Fire`) or repeating (`Tick`, which schedules the next
//! one). The process stops before some are due and after others, sometimes in
//! the middle of a fire: its event may be written while its row is not yet
//! removed. The next process boots with `start_runtime` and nobody touches the
//! records.
//!
//! Invariants:
//! - every armed `Fire` fires exactly once, never before it is due, and at its
//!   due time when that falls after the boot, or soon after the boot when the
//!   process was down at that time;
//! - a repeating `Tick` keeps going after the restart, never twice within one
//!   period;
//! - no row is left for a fired action, and a repeating record keeps one.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime;
//! dropping the runtime is the process exit. Scheduled-action rows are kept
//! with the journal in the simulated store, state timer rows in
//! [`SimTimerRows`]; both outlive a restart.

#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use world::{ProcessClock, SimTimerRows};

const SEEDS: u64 = 48;

/// How long `Start` and every `Tick` wait before their action fires.
const PERIOD: i64 = 600;

/// A fire after the boot must come at its due time; one that fell due while
/// the process was down, within this long of the boot (eight fire at once).
const BOOT_FIRE_WINDOW: i64 = 5;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Active", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Waiting", "Active", "Done"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
effect = ["schedule('Fire', 600)"]

[[action]]
name = "StartAt"
from = ["Idle"]
to = "Waiting"
params = ["due"]
effect = ["schedule_at('Fire', due)"]

[[action]]
name = "Activate"
from = ["Idle"]
to = "Active"
effect = ["schedule('Tick', 600)"]

[[action]]
name = "Tick"
from = ["Active"]
effect = ["schedule('Tick', 600)"]

[[action]]
name = "Fire"
from = ["Waiting", "Done"]
to = "Done"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="due" Type="Edm.String"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="StartAt" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="due" Type="Edm.String"/></Action>
<Action Name="Activate" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Tick" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

const TENANT: &str = "alpha";

/// What a record of the workload armed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    /// `Fire`, due at this time.
    Once(DateTime<Utc>),
    /// `Tick` every [`PERIOD`].
    Repeat,
}

/// What outlives a process: the journal, the timer rows and the clock.
struct World {
    events: SimEventStore,
    rows: Arc<SimTimerRows>,
    clock: Arc<ProcessClock>,
}

/// One server process.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    /// Boot the way every embedder does: specs, storage, then
    /// `start_runtime`.
    fn start(world: &World, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(CSDL).unwrap();
            registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Job", JOB)]);
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            let walk = state.start_runtime().await.expect("the first start");
            walk.await.unwrap();
            state
        });
        Self { runtime, state }
    }

    /// Exit: every task of this process stops.
    fn kill(self, world: &World, downtime: Duration) {
        self.runtime.block_on(async { world.clock.end(downtime) });
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// One seed's run: its trace, for the report of a broken invariant.
struct Run {
    seed: u64,
    trace: Vec<String>,
}

impl Run {
    fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }
}

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let `by` pass. The paused clock jumps from one timer to the next, so each
/// fires at its own instant.
async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
    settle().await;
}

/// Let time pass until `at`, waking exactly then.
async fn until(at: DateTime<Utc>) {
    tokio::time::sleep((at - sim_now()).to_std().unwrap_or_default()).await;
}

async fn act(state: &ServerState, id: &str, action: &str, params: serde_json::Value) -> bool {
    state
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "Job",
            id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
        .is_ok_and(|response| response.success)
}

/// Whether the journal `persistence_id` holds an event of `action`.
async fn journal_has(events: &SimEventStore, persistence_id: &str, action: &str) -> bool {
    events
        .read_events(persistence_id, 0)
        .await
        .is_ok_and(|events| events.iter().any(|event| event.event_type == action))
}

/// When each of the record's events of `action` was applied.
async fn fired_at(state: &ServerState, id: &str, action: &str) -> Vec<DateTime<Utc>> {
    state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Job", id)
        .await
        .map(|response| {
            response
                .state
                .events
                .iter()
                .filter(|event| event.action == action)
                .map(|event| event.timestamp)
                .collect()
        })
        .unwrap_or_default()
}

/// One seed's run. Ok: how many rows the stop left behind a fire whose event
/// was already written, which the boot fires again, and how it stopped.
fn run_seed(seed: u64) -> Result<(usize, String), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0x5c4e_d01e);
    let mut run = Run {
        seed,
        trace: Vec::new(),
    };
    let mut arms: BTreeMap<String, Arm> = BTreeMap::new();

    let first = Process::start(&world, &format!("sched-{seed}-a"));
    first.runtime.block_on(async {
        let state = &first.state;
        for i in 0..2 + rng.next_bound(5) {
            let id = format!("job-{i}");
            pass(Duration::from_secs(rng.next_bound(120) as u64)).await;
            state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", &id, json!({}))
                .await
                .map_err(|error| run.fail("records are created", error))?;
            let now = sim_now();
            let (action, params, arm) = match rng.next_bound(3) {
                0 => (
                    "Start",
                    json!({}),
                    Arm::Once(now + chrono::Duration::seconds(PERIOD)),
                ),
                1 => {
                    let due = now + chrono::Duration::seconds(rng.next_bound(7200) as i64);
                    (
                        "StartAt",
                        json!({ "due": due.to_rfc3339() }),
                        Arm::Once(due),
                    )
                }
                _ => ("Activate", json!({}), Arm::Repeat),
            };
            if !act(state, &id, action, params).await {
                return Err(run.fail("records arm their actions", format!("{id} {action}")));
            }
            run.trace.push(format!("{id}: {action} at {now}, {arm:?}"));
            arms.insert(id, arm);
        }
        settle().await;
        // Stop at a seeded time. Half the time, stop at an armed action's due
        // instant instead, a seeded few turns after its fire's event is
        // written: its row may not be removed yet, so the next boot fires it
        // again and only its idempotency key keeps that from running twice.
        let now = sim_now();
        let dues: Vec<(&String, DateTime<Utc>)> = arms
            .iter()
            .filter_map(|(id, arm)| match arm {
                Arm::Once(due) if *due > now => Some((id, *due)),
                _ => None,
            })
            .collect();
        if !dues.is_empty() && rng.next_bound(2) == 0 {
            let (id, due) = dues[rng.next_bound(dues.len())];
            until(due).await;
            let journal = format!("{TENANT}:Job:{id}");
            let mut waited = 0;
            while waited < 20_000 && !journal_has(&world.events, &journal, "Fire").await {
                tokio::task::yield_now().await;
                waited += 1;
            }
            let written = waited < 20_000;
            let row_then = world
                .events
                .list_scheduled_actions(TENANT)
                .await
                .unwrap()
                .iter()
                .any(|row| &row.entity_id == id);
            let turns = rng.next_bound(8);
            for _ in 0..turns {
                tokio::task::yield_now().await;
            }
            run.trace.push(format!(
                "stop at {} at {id}'s due: Fire written {written} after {waited} turns, \
                 row then {row_then}, stopped {turns} turns later",
                sim_now()
            ));
        } else {
            until(now + chrono::Duration::seconds(rng.next_bound(3600) as i64)).await;
            let turns = rng.next_bound(200);
            for _ in 0..turns {
                tokio::task::yield_now().await;
            }
            run.trace
                .push(format!("stop at {} after {turns} turns", sim_now()));
        }
        Ok::<(), String>(())
    })?;
    let downtime = Duration::from_secs(rng.next_bound(7200) as u64);
    first.kill(&world, downtime);
    let left_behind = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let mut left_behind = 0;
            let rows = world.events.list_scheduled_actions(TENANT).await.unwrap();
            for row in rows.iter().filter(|row| row.action == "Fire") {
                let journal = format!("{TENANT}:Job:{}", row.entity_id);
                left_behind += usize::from(journal_has(&world.events, &journal, "Fire").await);
            }
            left_behind
        });
    run.trace
        .push(format!("{left_behind} rows left behind a written fire"));

    let second = Process::start(&world, &format!("sched-{seed}-b"));
    let result = second.runtime.block_on(async {
        let state = &second.state;
        let boot = sim_now();
        run.trace.push(format!("boot at {boot}"));
        let last_due = arms
            .values()
            .filter_map(|arm| match arm {
                Arm::Once(due) => Some(*due),
                Arm::Repeat => None,
            })
            .max()
            .unwrap_or(boot)
            .max(boot);
        until(last_due + chrono::Duration::seconds(3 * PERIOD)).await;
        settle().await;
        let end = sim_now();
        let pending = world.events.list_scheduled_actions(TENANT).await.unwrap();
        for (id, arm) in &arms {
            let rows = pending.iter().filter(|row| &row.entity_id == id).count();
            if world.rows.row(TENANT, id).is_some() {
                return Err(run.fail("scheduled actions keep no state timer row", id.clone()));
            }
            match arm {
                Arm::Once(due) => {
                    let fires = fired_at(state, id, "Fire").await;
                    let [fire] = fires.as_slice() else {
                        return Err(run.fail(
                            "a scheduled action fires exactly once",
                            format!("{id} due {due}: fired at {fires:?}"),
                        ));
                    };
                    // Due after the boot, or fired before the stop: at its due
                    // time. Due while the process was down: soon after the boot.
                    let on_time = if *due > boot || *fire < boot {
                        (*fire - *due).num_seconds().abs() <= 1
                    } else {
                        (*fire - boot).num_seconds() <= BOOT_FIRE_WINDOW
                    };
                    if !on_time {
                        return Err(run.fail(
                            "a scheduled action fires when it is due",
                            format!("{id} due {due}, boot {boot}: fired at {fire}"),
                        ));
                    }
                    if rows != 0 {
                        return Err(
                            run.fail("a fired action keeps no row", format!("{id}: {rows} rows"))
                        );
                    }
                }
                Arm::Repeat => {
                    let ticks = fired_at(state, id, "Tick").await;
                    let after_boot = ticks.iter().filter(|tick| **tick >= boot).count();
                    let last = ticks.last().copied();
                    let close = ticks
                        .windows(2)
                        .find(|pair| (pair[1] - pair[0]).num_seconds() < PERIOD - 1);
                    if after_boot == 0
                        || last.is_none_or(|last| (end - last).num_seconds() > PERIOD + 10)
                    {
                        return Err(run.fail(
                            "a repeating action keeps going after a restart",
                            format!("{id}: ticks {ticks:?}, boot {boot}, end {end}"),
                        ));
                    }
                    if let Some(pair) = close {
                        return Err(run.fail(
                            "a repeating action fires once per period",
                            format!("{id}: ticks {} and {}", pair[0], pair[1]),
                        ));
                    }
                    if rows != 1 {
                        return Err(run.fail(
                            "a repeating action keeps one row",
                            format!("{id}: {rows} rows"),
                        ));
                    }
                }
            }
        }
        Ok(())
    });
    second.kill(&world, Duration::ZERO);
    let stop = run
        .trace
        .iter()
        .find(|line| line.starts_with("stop at"))
        .cloned()
        .unwrap_or_default();
    result.map(|()| (left_behind, stop))
}

#[test]
fn dst_scheduled_actions_outlive_a_restart() {
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    let mut left_behind = 0;
    let mut stops = Vec::new();
    for seed in 0..SEEDS {
        match run_seed(seed) {
            Ok((rows, stop)) => {
                left_behind += rows;
                stops.push(format!("seed {seed}: {stop}"));
            }
            Err(report) => {
                let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
                let entry = broken
                    .entry(invariant)
                    .or_insert_with(|| (Vec::new(), report));
                entry.0.push(seed);
            }
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
    // The duplicate a boot could fire exists only if some stop fell between
    // a fire's event and its row's removal; a harness that never stops there
    // cannot catch it.
    assert!(
        left_behind > 0,
        "no stop over {SEEDS} seeds left a row behind a written fire:\n{}",
        stops.join("\n")
    );
}
