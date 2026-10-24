//! DST: a record's armed timer follows its current state.
//!
//! Workload, per seed: records in two tenants enter a timed state; some are
//! poked, deleted, passivated and rehydrated; tenant `alpha` hot-swaps to a
//! spec under which replay lands its records in another timed state (`Start`
//! now goes to `Review`, not `Waiting`); the server restarts, so records that
//! were never snapshotted replay into the new state while their timer rows
//! still name the old one.
//!
//! Invariants:
//! - after a record rehydrates, it has exactly one armed timer row, for its
//!   current state, when that state declares a timeout, and none otherwise
//!   (none for a deleted record);
//! - timers fire only in their state: every `Expire` left `Waiting` and every
//!   `Approve` left `Review`, once;
//! - with nobody touching the records after the restart, every record still
//!   reaches `Done` on its timers alone.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime;
//! dropping the runtime is the process exit. The simulation clock reads that
//! paused clock, so `sim_now()` and timer sleeps share one virtual time. The
//! event journal is the simulated store; timer rows live in [`SimTimerRows`],
//! both shared across restarts.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, SimClock, install_sim_context,
};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SEEDS: u64 = 48;

/// The first spec: `Start` enters `Waiting`. `Expire` and `Approve` are
/// accepted from every live state, so a timer that fired outside its state
/// would leave an event saying so instead of being refused.
const JOB_V1: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Review", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Poke"
from = ["Waiting", "Review"]

[[action]]
name = "Expire"
from = ["Waiting", "Review", "Done"]
to = "Done"

[[action]]
name = "Approve"
from = ["Waiting", "Review", "Done"]
to = "Done"

[[state_timeout]]
state = "Waiting"
after_seconds = 3600
on_timeout = "Expire"

[[state_timeout]]
state = "Review"
after_seconds = 1800
on_timeout = "Approve"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Poke" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Approve" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// The second spec: replay of a stored `Start` now lands in `Review`.
fn job_v2() -> String {
    JOB_V1.replacen(
        "from = [\"Idle\"]\nto = \"Waiting\"",
        "from = [\"Idle\"]\nto = \"Review\"",
        1,
    )
}

/// The declared timer of each state: the action it fires.
fn timer_action(state: &str) -> Option<&'static str> {
    match state {
        "Waiting" => Some("Expire"),
        "Review" => Some("Approve"),
        _ => None,
    }
}

/// Timer rows, kept the way the SQL stores keep them: one per record, a row
/// armed at a lower journal sequence never replacing a higher one, removed
/// only by the arming that wrote it.
#[derive(Default)]
struct SimTimerRows(Mutex<BTreeMap<(String, String, String), StateTimerRecord>>);

impl SimTimerRows {
    fn row(&self, tenant: &str, id: &str) -> Option<StateTimerRecord> {
        let rows = self.0.lock().unwrap();
        rows.get(&(tenant.to_string(), "Job".to_string(), id.to_string()))
            .cloned()
    }
}

#[async_trait::async_trait]
impl StateTimerStore for SimTimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        let key = (
            timer.tenant.clone(),
            timer.entity_type.clone(),
            timer.entity_id.clone(),
        );
        let mut rows = self.0.lock().unwrap();
        if rows
            .get(&key)
            .is_none_or(|row| timer.sequence_nr >= row.sequence_nr)
        {
            rows.insert(key, timer.clone());
        }
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let key = (
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        let mut rows = self.0.lock().unwrap();
        if rows.get(&key).is_some_and(|row| row.arm_id == arm_id) {
            rows.remove(&key);
        }
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        let rows = self.0.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| row.tenant == tenant)
            .cloned()
            .collect())
    }
}

/// Simulation time: the paused clock of the running process, carried across
/// restarts with the time the server was down.
struct ProcessClock {
    epoch: DateTime<Utc>,
    base: Mutex<(Option<tokio::time::Instant>, Duration)>,
}

impl ProcessClock {
    fn new() -> Self {
        Self {
            epoch: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
            base: Mutex::new((None, Duration::ZERO)),
        }
    }

    /// Start counting from this process's clock. Call inside its runtime.
    fn begin(&self) {
        self.base.lock().unwrap().0 = Some(tokio::time::Instant::now());
    }

    /// Stop counting, and add `downtime` before the next process. Call inside
    /// the ending process's runtime.
    fn end(&self, downtime: Duration) {
        let mut base = self.base.lock().unwrap();
        let ran = base
            .0
            .take()
            .map(|origin| origin.elapsed())
            .unwrap_or_default();
        base.1 += ran + downtime;
    }
}

impl SimClock for ProcessClock {
    fn now(&self) -> DateTime<Utc> {
        let base = self.base.lock().unwrap();
        let ran = base.0.map(|origin| origin.elapsed()).unwrap_or_default();
        self.epoch + chrono::Duration::from_std(base.1 + ran).unwrap()
    }

    fn tick(&self) -> u64 {
        0
    }
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
    /// Boot the way `temper serve` does: specs, storage, then the re-arm of
    /// the timers the last process left waiting.
    fn start(world: &World, alpha_spec: &str, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            for (tenant, spec) in [("alpha", alpha_spec), ("beta", JOB_V1)] {
                let csdl = parse_csdl(CSDL).unwrap();
                registry.register_tenant(tenant, csdl, CSDL.to_string(), &[("Job", spec)]);
            }
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            state.rearm_state_timers().await;
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

/// A record of the workload.
#[derive(Clone)]
struct Job {
    tenant: TenantId,
    id: String,
}

/// One seed's run: its trace and the first invariant it broke.
struct Run {
    seed: u64,
    trace: Vec<String>,
    deleted: BTreeSet<(String, String)>,
}

impl Run {
    fn note(&mut self, line: String) {
        self.trace.push(line);
    }

    fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }

    fn is_deleted(&self, job: &Job) -> bool {
        self.deleted
            .contains(&(job.tenant.to_string(), job.id.clone()))
    }
}

/// Let every task that can run, run: due timers fire, spawned reconciles
/// finish. The test task stays runnable, so the paused clock never moves.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

async fn advance(by: Duration) {
    tokio::time::advance(by).await;
    settle().await;
}

async fn act(state: &ServerState, job: &Job, action: &str) -> bool {
    state
        .dispatch_tenant_action(
            &job.tenant,
            "Job",
            &job.id,
            action,
            json!({}),
            &AgentContext::default(),
        )
        .await
        .is_ok_and(|response| response.success)
}

async fn status(state: &ServerState, job: &Job) -> String {
    state
        .get_tenant_entity_state(&job.tenant, "Job", &job.id)
        .await
        .map(|response| response.state.status)
        .unwrap_or_else(|error| format!("<unreadable: {error}>"))
}

/// Passivate a random part of the records: the rest are read first, so only
/// the untouched ones are idle long enough.
async fn passivate_some(
    state: &ServerState,
    jobs: &[Job],
    rng: &mut DeterministicRng,
    run: &mut Run,
) {
    advance(Duration::from_secs(301)).await;
    let mut kept = Vec::new();
    for job in jobs.iter().filter(|job| !run.is_deleted(job)) {
        if rng.chance(0.5) {
            status(state, job).await;
            kept.push(job.id.clone());
        }
    }
    while state.passivate_idle_actors().await > 0 {}
    settle().await;
    run.note(format!("passivated all but {kept:?}"));
}

/// Invariant: after `jobs` rehydrate, each has exactly the timer row its
/// current state declares.
async fn check_rows_follow_state(
    state: &ServerState,
    world: &World,
    jobs: &[Job],
    run: &Run,
    when: &str,
) -> Result<(), String> {
    for job in jobs.iter().filter(|job| !run.is_deleted(job)) {
        status(state, job).await;
    }
    settle().await;
    for job in jobs {
        let row = world.rows.row(job.tenant.as_str(), &job.id);
        let current = if run.is_deleted(job) {
            "<deleted>".to_string()
        } else {
            status(state, job).await
        };
        let wanted = timer_action(&current).map(|_| current.as_str());
        if row.as_ref().map(|row| row.state.as_str()) != wanted {
            return Err(run.fail(
                "timer rows follow the current state",
                format!(
                    "{when}: {}/{} is {current} but its timer row is {:?}",
                    job.tenant,
                    job.id,
                    row.map(|row| row.state)
                ),
            ));
        }
    }
    Ok(())
}

/// Invariants at the end: every record reached `Done` on its timers, every
/// timer fired once and in its own state, and no row is left.
async fn check_all_done(
    state: &ServerState,
    world: &World,
    jobs: &[Job],
    run: &Run,
) -> Result<(), String> {
    for job in jobs.iter().filter(|job| !run.is_deleted(job)) {
        let response = state
            .get_tenant_entity_state(&job.tenant, "Job", &job.id)
            .await
            .map_err(|error| run.fail("records stay readable", error))?;
        let record = &response.state;
        let fires: Vec<_> = record
            .events
            .iter()
            .filter(|event| event.action == "Expire" || event.action == "Approve")
            .collect();
        for event in &fires {
            if timer_action(&event.from_status) != Some(event.action.as_str()) {
                return Err(run.fail(
                    "timers fire only in their state",
                    format!(
                        "{}/{}: {} fired in {}",
                        job.tenant, job.id, event.action, event.from_status
                    ),
                ));
            }
        }
        if record.status != "Done" || fires.len() != 1 {
            return Err(run.fail(
                "every record leaves its timed state on its timer",
                format!(
                    "{}/{} is {} after {} timer fires",
                    job.tenant,
                    job.id,
                    record.status,
                    fires.len()
                ),
            ));
        }
    }
    for job in jobs {
        if let Some(row) = world.rows.row(job.tenant.as_str(), &job.id) {
            return Err(run.fail(
                "no timer outlives its state",
                format!("{}/{} keeps a row for {}", job.tenant, job.id, row.state),
            ));
        }
    }
    Ok(())
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::default()),
        clock,
    };
    let mut rng = DeterministicRng::new(seed);
    let mut run = Run {
        seed,
        trace: Vec::new(),
        deleted: BTreeSet::new(),
    };
    let mut jobs = Vec::new();
    for tenant in ["alpha", "beta"] {
        for i in 0..3 + rng.next_bound(3) {
            jobs.push(Job {
                tenant: TenantId::new(tenant),
                id: format!("job-{i}"),
            });
        }
    }
    let v2 = job_v2();

    let first = Process::start(&world, JOB_V1, &format!("follow-{seed}-a"));
    first.runtime.block_on(async {
        let state = &first.state;
        for job in &jobs {
            advance(Duration::from_secs(rng.next_bound(60) as u64)).await;
            state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .map_err(|error| run.fail("records are created", error))?;
            if !act(state, job, "Start").await {
                return Err(run.fail("records start", format!("{}/{}", job.tenant, job.id)));
            }
        }
        run.note(format!("started {} records", jobs.len()));
        for job in &jobs {
            if rng.chance(0.3) {
                act(state, job, "Poke").await;
                run.note(format!("poked {}/{}", job.tenant, job.id));
            }
        }
        for tenant in ["alpha", "beta"] {
            if rng.chance(0.3) {
                let of_tenant: Vec<_> = jobs
                    .iter()
                    .filter(|job| job.tenant.as_str() == tenant)
                    .collect();
                let job = of_tenant[rng.next_bound(of_tenant.len())];
                state
                    .delete_tenant_entity(&job.tenant, "Job", &job.id)
                    .await
                    .map_err(|error| run.fail("records are deleted", error))?;
                run.deleted.insert((tenant.to_string(), job.id.clone()));
                run.note(format!("deleted {tenant}/{}", job.id));
            }
        }
        settle().await;
        if rng.chance(0.7) {
            passivate_some(state, &jobs, &mut rng, &mut run).await;
        }

        {
            let mut registry = state.registry.write().unwrap();
            let csdl = parse_csdl(CSDL).unwrap();
            registry.register_tenant("alpha", csdl, CSDL.to_string(), &[("Job", v2.as_str())]);
        }
        state.follow_spec_changes().await;
        settle().await;
        run.note("hot-swapped alpha: Start now enters Review".to_string());

        let live: Vec<Job> = jobs
            .iter()
            .filter(|job| !run.is_deleted(job))
            .cloned()
            .collect();
        for job in &live {
            match rng.next_bound(3) {
                0 => {}
                1 => {
                    act(state, job, "Poke").await;
                    run.note(format!("poked {}/{}", job.tenant, job.id));
                }
                _ => {
                    status(state, job).await;
                    run.note(format!("read {}/{}", job.tenant, job.id));
                }
            }
        }
        if rng.chance(0.7) {
            passivate_some(state, &jobs, &mut rng, &mut run).await;
        }
        check_rows_follow_state(state, &world, &jobs, &run, "after the hot swap").await
    })?;

    let downtime = Duration::from_secs(rng.next_bound(600) as u64);
    run.note(format!("restart after {}s down", downtime.as_secs()));
    first.kill(&world, downtime);

    let second = Process::start(&world, &v2, &format!("follow-{seed}-b"));
    second.runtime.block_on(async {
        let state = &second.state;
        let looked: Vec<Job> = jobs.iter().filter(|_| rng.chance(0.4)).cloned().collect();
        run.note(format!(
            "rehydrated {:?}",
            looked
                .iter()
                .map(|job| format!("{}/{}", job.tenant, job.id))
                .collect::<Vec<_>>()
        ));
        check_rows_follow_state(state, &world, &looked, &run, "after the restart").await?;
        // Nobody touches the records: their timers alone must finish them.
        for _ in 0..360 {
            advance(Duration::from_secs(30)).await;
        }
        check_all_done(state, &world, &jobs, &run).await
    })?;
    second.kill(&world, Duration::ZERO);
    Ok(())
}

#[test]
fn dst_state_timers_follow_the_current_state() {
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed) {
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
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}
