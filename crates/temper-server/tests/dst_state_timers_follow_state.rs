//! DST: a record's armed timer follows its current state.
//!
//! Workload, per seed: records in three tenants enter `Waiting`, timed in
//! `alpha` and `beta` and untimed in `gamma`; some are poked, deleted,
//! passivated and rehydrated. An install then changes two specs: `alpha`'s,
//! under which replay lands its records in another timed state (`Start` now
//! goes to `Review`, not `Waiting`), and `gamma`'s, which gains the timeouts.
//! In some seeds the process stops before the install's walk reaches `gamma`.
//! The server restarts, so records that were never snapshotted replay into
//! the new state while their timer rows still name the old one, and `gamma`'s
//! records may have no row at all.
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
//! both shared across restarts. Each row write lands after a seeded delay, so
//! the writes of overlapping arms and cancellations land in any order.

#[path = "dst_state_timers_follow_state/invariants.rs"]
mod invariants;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use invariants::{check_all_done, check_rows_follow_state};
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use world::{ProcessClock, SimTimerRows};

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

/// `Job` with no timeout: records wait in `Waiting` and `Review` for good.
fn job_untimed() -> String {
    let untimed = JOB_V1[..JOB_V1.find("[[state_timeout]]").unwrap()].to_string();
    untimed.replacen(
        "allow_indefinite_states = [\"Idle\", \"Done\"]",
        "allow_indefinite_states = [\"Idle\", \"Waiting\", \"Review\", \"Done\"]",
        1,
    )
}

/// The second spec: replay of a stored `Start` now lands in `Review`.
fn job_v2() -> String {
    JOB_V1.replacen(
        "from = [\"Idle\"]\nto = \"Waiting\"",
        "from = [\"Idle\"]\nto = \"Review\"",
        1,
    )
}

/// The tenants of the workload: `gamma` starts with [`job_untimed`].
const TENANTS: [&str; 3] = ["alpha", "beta", "gamma"];

/// Install `spec` as `tenant`'s `Job`, replacing the one it had.
fn install(state: &ServerState, tenant: &str, spec: &str) {
    let mut registry = state.registry.write().unwrap();
    let csdl = parse_csdl(CSDL).unwrap();
    registry.register_tenant(tenant, csdl, CSDL.to_string(), &[("Job", spec)]);
}

/// The declared timer of each state: the action it fires.
fn timer_action(state: &str) -> Option<&'static str> {
    match state {
        "Waiting" => Some("Expire"),
        "Review" => Some("Approve"),
        _ => None,
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
    /// the timers the last process left waiting, and the walk of every timed
    /// type's records that follows it (awaited here, in the background in
    /// `temper serve`).
    fn start(world: &World, alpha_spec: &str, gamma_spec: &str, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            for (tenant, spec) in [
                ("alpha", alpha_spec),
                ("beta", JOB_V1),
                ("gamma", gamma_spec),
            ] {
                let csdl = parse_csdl(CSDL).unwrap();
                registry.register_tenant(tenant, csdl, CSDL.to_string(), &[("Job", spec)]);
            }
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            state.rearm_state_timers().await.await.unwrap();
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

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed);
    let mut run = Run {
        seed,
        trace: Vec::new(),
        deleted: BTreeSet::new(),
    };
    let mut jobs = Vec::new();
    for tenant in TENANTS {
        for i in 0..3 + rng.next_bound(3) {
            jobs.push(Job {
                tenant: TenantId::new(tenant),
                id: format!("job-{i}"),
            });
        }
    }
    let v2 = job_v2();
    let untimed = job_untimed();

    let first = Process::start(&world, JOB_V1, &untimed, &format!("follow-{seed}-a"));
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
        for tenant in TENANTS {
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

        install(state, "alpha", &v2);
        let gamma_followed = rng.chance(0.5);
        if gamma_followed {
            install(state, "gamma", JOB_V1);
            state.follow_spec_changes().await;
        } else {
            state.follow_spec_changes().await;
            // The process stops before the install's walk reaches gamma.
            install(state, "gamma", JOB_V1);
            state.registry.write().unwrap().take_changed_types();
        }
        settle().await;
        run.note(format!(
            "installed: alpha's Start now enters Review, gamma gains timeouts{}",
            if gamma_followed {
                ""
            } else {
                " (its walk never ran)"
            }
        ));

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
        let followed: Vec<Job> = jobs
            .iter()
            .filter(|job| gamma_followed || job.tenant.as_str() != "gamma")
            .cloned()
            .collect();
        check_rows_follow_state(state, &world, &followed, &run, "after the install").await
    })?;

    let downtime = Duration::from_secs(rng.next_bound(600) as u64);
    run.note(format!("restart after {}s down", downtime.as_secs()));
    first.kill(&world, downtime);

    let second = Process::start(&world, &v2, JOB_V1, &format!("follow-{seed}-b"));
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
        for job in &looked {
            status(state, job).await;
        }
        settle().await;
        // The boot walk has run: every record follows its state, read or not.
        check_rows_follow_state(state, &world, &jobs, &run, "after the restart").await?;
        // Nobody touches the records: their timers alone must finish them.
        for _ in 0..360 {
            advance(Duration::from_secs(30)).await;
        }
        check_all_done(state, &world, &jobs, &run).await
    })?;
    second.kill(&world, Duration::ZERO);
    Ok(())
}

#[path = "dst_state_timers_follow_state/read_failure.rs"]
mod read_failure;

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
