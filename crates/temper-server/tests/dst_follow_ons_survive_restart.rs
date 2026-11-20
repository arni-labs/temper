//! DST: a write a committed move owes lands exactly once, even when the
//! process stops before it lands.
//!
//! When a move commits, the kernel owes the writes that follow from it: a
//! module's answer, a reaction on another record, a child the move spawns,
//! the move a state timer fires. Nobody will ask for them again. A store
//! that refuses one is retried in the process (`dst_owed_writes_land_after_refusal`);
//! here the process stops while the store still refuses, so whatever the
//! process held in memory is gone, and the next process must still land each
//! owed write, once.
//!
//! Workload, per case and seed: a `Job` takes the move that sets one owed
//! write off. The store stops taking writes at a seeded point (`Stop`):
//! right after the move commits, so the owed write never lands in this
//! process; or right after the owed write itself lands, so only what the
//! kernel writes after it (its bookkeeping) is refused. A seeded stretch
//! later, up to an hour, the process exits. The next process boots on a
//! store that takes writes, the way every server starts (`start_runtime`),
//! and nobody touches the records.
//!
//! Invariants, per seed:
//! - the move that sets the write off lands exactly once;
//! - within [`RECOVERY_WINDOW`] of the boot, each owed write has landed
//!   exactly once: never lost to the stop, never applied twice by a boot
//!   that finishes what already landed.
//!
//! So that a run means something: a control run with no stop must land the
//! owed write exactly once, and each seed stopped after the move checks that
//! the owed write had not landed when the process exited and that the store
//! refused an append to its journal (it was tried, and the process held it).
//!
//! Each process runs in its own single-threaded, paused-clock Tokio
//! runtime; dropping the runtime is the process exit. The journal and the
//! rows written with it are kept in the simulated store, state timer rows
//! in [`SimTimerRows`]; both outlive the exit.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_owed_writes_land_after_refusal/specs.rs"]
mod specs;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use process_clock::ProcessClock;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use world::SimTimerRows;

const SEEDS: u64 = 24;
const TENANT: &str = "default";
/// The record that takes the move, and the id of its `Ledger`.
const JOB: &str = "job";
/// The `Child` that `Spawn` creates.
const CHILD: &str = "child";
/// The longest the first process runs on after the store stops.
const LONGEST_STRETCH_SECS: usize = 3_600;
/// How soon after the boot an owed write has landed: the longest wait
/// between two tries of an owed write (`owed_writes::LAST_RETRY_WAIT`, an
/// hour), twice, and a minute for the work of the tries.
const RECOVERY_WINDOW: Duration = Duration::from_secs(2 * 3_600 + 60);

// The window holds the state timer's whole wait, so a timer owed at the exit
// has fallen due inside it.
const _: () = assert!(RECOVERY_WINDOW.as_secs() > specs::TIMER_SECS);

/// The write a move sets off, which the kernel owes once the move commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owed {
    /// `Ask` runs the module in the background: its `Answer`.
    BackgroundAnswer,
    /// `Note` counts itself on the record's `Ledger`: the `Count`.
    Reaction,
    /// `Spawn` creates a `Child`: its creation and its `Init`.
    ChildSpawn,
    /// `Hold` enters `Waiting`: the `Expire` its timer fires. Its row is
    /// written with the move, so it is the control among the cases.
    TimerFire,
}

impl Owed {
    /// The `Job` move that sets the write off, and its parameters.
    fn trigger(self) -> (&'static str, serde_json::Value) {
        match self {
            Self::BackgroundAnswer => ("Ask", json!({})),
            Self::Reaction => ("Note", json!({})),
            Self::ChildSpawn => ("Spawn", json!({ "child_id": CHILD })),
            Self::TimerFire => ("Hold", json!({})),
        }
    }

    /// The record that owes the write, and the events its journal must hold
    /// exactly once.
    fn owing(self) -> (Record, &'static [&'static str]) {
        match self {
            Self::BackgroundAnswer => (Record::new("Job", JOB), &["Answer"]),
            Self::Reaction => (Record::new("Ledger", JOB), &["Count"]),
            Self::ChildSpawn => (Record::new("Child", CHILD), &["Created", "Init"]),
            Self::TimerFire => (Record::new("Job", JOB), &["Expire"]),
        }
    }
}

/// Where the store stops taking writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Right after the move commits: the owed write never lands in the first
    /// process.
    AfterMove,
    /// Right after the owed write's first append lands: what the kernel
    /// writes after it is refused, and the next boot must not apply the owed
    /// write again.
    AfterOwedWrite,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    entity_type: &'static str,
    id: &'static str,
}

impl Record {
    const fn new(entity_type: &'static str, id: &'static str) -> Self {
        Self { entity_type, id }
    }

    fn journal(self) -> String {
        format!("{TENANT}:{}:{}", self.entity_type, self.id)
    }
}

/// How many of a record's events are of `event_type`.
fn count(store: &SimEventStore, record: Record, event_type: &str) -> usize {
    store
        .dump_journal(&record.journal())
        .iter()
        .filter(|event| event.event_type == event_type)
        .count()
}

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
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

/// What outlives a process: the journal and its rows, the timer rows and
/// the clock.
struct World {
    store: SimEventStore,
    rows: Arc<SimTimerRows>,
    clock: Arc<ProcessClock>,
}

/// One server process, in its own runtime.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    /// Boot the way every embedder does: specs, storage, policy, the module,
    /// then `start_runtime`.
    fn start(world: &World, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .expect("runtime builds");
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(specs::CSDL).expect("CSDL parses");
            registry.register_tenant(
                TENANT,
                csdl,
                specs::CSDL.to_string(),
                &[
                    ("Job", specs::JOB),
                    ("Ledger", specs::LEDGER),
                    ("Child", specs::CHILD),
                ],
            );
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.store.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            // A reaction and a callback are authorized as moves of their
            // own: with no tenant policy, Cedar refuses them.
            state
                .authz
                .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
                .expect("the tenant policy parses");
            let hash = state
                .wasm_engine
                .compile_and_cache(specs::oracle_module(&json!(42)).as_bytes())
                .expect("the module compiles");
            state.register_wasm_module(&TenantId::new(TENANT), "oracle", &hash);
            let walk = state.start_runtime().await.expect("the runtime starts");
            walk.await.expect("the boot walk ends");
            state
        });
        Self { runtime, state }
    }

    /// Exit after `downtime`: every task of this process stops, and whatever
    /// it held in memory is gone.
    fn exit(self, world: &World, downtime: Duration) {
        self.runtime.block_on(async { world.clock.end(downtime) });
        let Self { runtime, state } = self;
        drop(state);
        drop(runtime);
    }
}

/// Dispatch the move that sets `owed` off, and say how it was answered.
async fn set_off(state: &ServerState, owed: Owed) -> String {
    let (action, params) = owed.trigger();
    let caller = AgentContext::default();
    let options = DispatchExtOptions {
        agent_ctx: &caller,
        await_integration: false,
        await_reactions: true,
    };
    match state
        .dispatch_tenant_action_ext(&TenantId::new(TENANT), "Job", JOB, action, params, options)
        .await
    {
        Ok(response) => format!("success {}, error {:?}", response.success, response.error),
        Err(error) => format!("dispatch error {error}"),
    }
}

/// One seed of `owed` stopped at `stop` (none: the control), and the first
/// broken invariant.
fn run_seed(owed: Owed, stop: Option<Stop>, seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        store: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0x0f_011_0e5);
    for _ in 0..8 {
        rng.next_u64();
    }
    let stretch = Duration::from_secs(1 + rng.next_bound(LONGEST_STRETCH_SECS) as u64);
    let downtime = Duration::from_secs(1 + rng.next_bound(600) as u64);
    let (action, _) = owed.trigger();
    let (owing, owed_events) = owed.owing();
    let job = Record::new("Job", JOB);
    let mut run = Run {
        seed,
        trace: Vec::new(),
    };

    let first = Process::start(&world, &format!("first-{seed}"));
    let held = first.runtime.block_on(async {
        let tenant = TenantId::new(TENANT);
        let mut records = vec![job];
        if owed == Owed::Reaction {
            records.push(owing);
        }
        for record in records {
            first
                .state
                .get_or_create_tenant_entity(
                    &tenant,
                    record.entity_type,
                    record.id,
                    json!({ "Id": record.id }),
                )
                .await
                .map_err(|error| run.fail("records are created", error.to_string()))?;
        }
        match stop {
            Some(Stop::AfterMove) => world.store.stop_writes_after_next_append(&job.journal()),
            Some(Stop::AfterOwedWrite) => {
                world.store.stop_writes_after_next_append(&owing.journal())
            }
            None => {}
        }
        let answer = set_off(&first.state, owed).await;
        run.trace.push(format!(
            "{action} (stop: {stop:?}): {answer}; the process exits {}s later",
            stretch.as_secs()
        ));
        tokio::time::sleep(stretch).await;
        settle().await;
        let landed: Vec<usize> = owed_events
            .iter()
            .map(|event| count(&world.store, owing, event))
            .collect();
        let refused = world.store.refused_appends(&owing.journal());
        run.trace.push(format!(
            "at the exit {} holds {owed_events:?} {landed:?} times, {refused} appends to it refused; \
             the job's state timer row is in {:?}",
            owing.journal(),
            world.rows.row(TENANT, JOB).map(|row| row.state)
        ));
        if stop == Some(Stop::AfterMove) {
            if landed.iter().any(|times| *times > 0) {
                return Err(run.fail(
                    "the owed write has not landed when the process exits (harness)",
                    format!("{owed_events:?} landed {landed:?} times"),
                ));
            }
            // The timer's fire falls due after the exit on most seeds: it is
            // owed by its row, not tried in the process.
            if refused == 0 && owed != Owed::TimerFire {
                return Err(run.fail(
                    "the owed write was tried while the store took no writes (harness)",
                    format!("no append to {} was refused", owing.journal()),
                ));
            }
        }
        Ok::<(), String>(())
    });
    first.exit(&world, downtime);
    held?;

    world.store.take_writes_again();
    let second = Process::start(&world, &format!("second-{seed}"));
    let ended = second.runtime.block_on(async {
        tokio::time::sleep(RECOVERY_WINDOW).await;
        settle().await;
        let moves = count(&world.store, job, action);
        if moves != 1 {
            return Err(run.fail(
                "the move that sets the write off lands exactly once",
                format!("{action} landed {moves} times"),
            ));
        }
        for event in owed_events {
            let landed = count(&world.store, owing, event);
            run.trace.push(format!(
                "{}s after the boot {event} has landed {landed} times",
                RECOVERY_WINDOW.as_secs()
            ));
            if landed != 1 {
                return Err(run.fail(
                    "an owed write lands exactly once across a restart",
                    format!("{event} landed {landed} times"),
                ));
            }
        }
        Ok::<(), String>(())
    });
    second.exit(&world, Duration::ZERO);
    ended
}

/// Run `owed` stopped at `stop` over every seed and fail with each broken
/// invariant, the seeds that broke it and the first report.
fn check_over_seeds(owed: Owed, stop: Stop) {
    if let Err(report) = run_seed(owed, None, 0) {
        panic!("{owed:?}: the owed write lands when nothing stops (harness): {report}");
    }
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(owed, Some(stop), seed) {
            let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
            let entry = broken
                .entry(invariant)
                .or_insert_with(|| (Vec::new(), report));
            entry.0.push(seed);
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "{owed:?}, {stop:?}: invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}

#[test]
fn dst_a_reaction_owed_when_the_process_stops_lands_once_after_the_boot() {
    check_over_seeds(Owed::Reaction, Stop::AfterMove);
}

#[test]
fn dst_a_reaction_that_landed_before_the_process_stopped_is_not_applied_again() {
    check_over_seeds(Owed::Reaction, Stop::AfterOwedWrite);
}

#[test]
fn dst_a_child_owed_when_the_process_stops_is_created_once_after_the_boot() {
    check_over_seeds(Owed::ChildSpawn, Stop::AfterMove);
}

#[test]
fn dst_a_child_created_before_the_process_stopped_is_not_created_again() {
    check_over_seeds(Owed::ChildSpawn, Stop::AfterOwedWrite);
}

#[test]
fn dst_a_module_answer_owed_when_the_process_stops_lands_once_after_the_boot() {
    check_over_seeds(Owed::BackgroundAnswer, Stop::AfterMove);
}

#[test]
fn dst_a_timer_owed_when_the_process_stops_fires_once_after_the_boot() {
    check_over_seeds(Owed::TimerFire, Stop::AfterMove);
}
