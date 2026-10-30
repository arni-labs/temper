//! DST: a timer acts with the standing its setter has when it fires.
//!
//! A timer fires as whoever set it. Its move is the app's own, so Cedar is
//! not asked about the move: it is asked whether the setter may still read
//! the record. The tenant's Cedar here names its people, as an app's does: they
//! may read a `Job`, `Start` it and `Hold` it, and nobody is permitted the
//! timers' own moves, `Expire` and `Take`.
//!
//! Workload, per seed: records are set going at seeded times, each by one
//! setter, with a state timer (`Start` enters `Waiting`, whose timeout makes
//! `Expire`) or a scheduled action (`Hold` schedules `Take`). The setters:
//! - a user who stays in the tenant's Cedar, by themselves or through an
//!   agent acting for them;
//! - a user who leaves it after setting the timer and before it fires, by
//!   themselves or through an agent;
//! - the app's own service, which Cedar names nowhere;
//! - an internal caller with no security context.
//!
//! The tenant's Cedar outlives a process, as stored policies do. Some seeds
//! restart the server before the timers fire, with or without them falling
//! due while it is down; every seed restarts once more after they fired.
//!
//! Invariants, long after every timer was due, and again after the last
//! restart:
//! - a timer whose setter may still read the record, or that acts for no one,
//!   makes its move exactly once;
//! - a timer whose setter may no longer read the record never makes its move,
//!   and says so in a log line naming the setter;
//! - such a state timer keeps its row, armed as its setter, so no later
//!   arming fires it as anyone else; such a scheduled action leaves no row
//!   and no live timer.
//!
//! Each server runs in its own single-threaded, paused-clock Tokio runtime,
//! and `sim_now()` reads that clock.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use process_clock::ProcessClock;
use serde_json::json;
use temper_authz::{PrincipalKind, SecurityContext};
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
use tracing_subscriber::prelude::*;
use world::SimTimerRows;

const SEEDS: u64 = 16;
const TENANT: &str = "alpha";
/// Both timers' wait: every record is set going, and every leaver leaves,
/// well within it.
const WAIT: Duration = Duration::from_secs(600);
/// How long the run goes on after the timers fell due.
const HORIZON: Duration = Duration::from_secs(4 * 3600);

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Held", "Done", "Taken"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Held", "Done", "Taken"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting"]
to = "Done"

[[state_timeout]]
state = "Waiting"
after_seconds = 600
on_timeout = "Expire"

[[action]]
name = "Hold"
from = ["Idle"]
to = "Held"
effect = ["schedule('Take', 600)"]

[[action]]
name = "Take"
kind = "internal"
from = ["Held"]
to = "Taken"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Hold" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Take" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// The agent a user may act through.
const AGENT: &str = "katagami-mcp";

/// Which timer a record is set going with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    /// `Start`: `Waiting`'s state timer makes `Expire`.
    State,
    /// `Hold`: a scheduled `Take`.
    Scheduled,
}

impl Timer {
    /// The move that sets it, the move it makes, the place the record waits
    /// in and the place its move takes the record to.
    fn moves(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::State => ("Start", "Expire", "Waiting", "Done"),
            Self::Scheduled => ("Hold", "Take", "Held", "Taken"),
        }
    }
}

/// Whether a user sets a timer themselves or through an agent acting for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum By {
    Themselves,
    Agent,
}

/// Who sets a record's timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Setter {
    /// A user who stays in the tenant's Cedar.
    Stays(By),
    /// A user who leaves the tenant's Cedar before the timer fires.
    Leaves(By),
    /// The app's own service, which the Cedar names nowhere.
    App,
    /// An internal caller with no security context.
    Internal,
}

impl Setter {
    /// The identity the record's user `user` sets the timer as.
    fn caller(self, user: &str) -> AgentContext {
        let verified = |id: &str, kind, agent_type, acting_for| AgentContext {
            security_ctx: Some(SecurityContext::from_verified_jwt(
                id, kind, agent_type, acting_for, None, None,
            )),
            ..Default::default()
        };
        match self {
            Self::Stays(By::Themselves) | Self::Leaves(By::Themselves) => {
                verified(user, PrincipalKind::Customer, None, None)
            }
            Self::Stays(By::Agent) | Self::Leaves(By::Agent) => {
                verified(AGENT, PrincipalKind::Agent, Some("contributor"), Some(user))
            }
            Self::App => AgentContext::for_service("app"),
            Self::Internal => AgentContext::default(),
        }
    }

    /// The principal a withheld fire names as its setter.
    fn principal(self, user: &str) -> String {
        match self {
            Self::Stays(By::Agent) | Self::Leaves(By::Agent) => AGENT.to_string(),
            _ => user.to_string(),
        }
    }

    fn leaves(self) -> bool {
        matches!(self, Self::Leaves(_))
    }
}

/// What outlives a process: the journal, the timer rows, the clock and the
/// tenant's people.
struct World {
    events: SimEventStore,
    rows: Arc<SimTimerRows>,
    clock: Arc<ProcessClock>,
    people: Mutex<BTreeSet<String>>,
}

impl World {
    /// The tenant's Cedar: its people may read a `Job`, `Start` it and
    /// `Hold` it, themselves or through an agent acting for them.
    fn cedar(&self) -> String {
        let people = self
            .people
            .lock()
            .unwrap()
            .iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"permit(principal, action in [Action::"read", Action::"Start", Action::"Hold"], resource is Job)
when {{ (principal is Customer && [{people}].contains(principal.id)) ||
        (context has actingFor && [{people}].contains(context.actingFor)) }};"#
        )
    }
}

/// One server process.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    /// Boot: specs, storage, the tenant's Cedar as stored, then
    /// `start_runtime`, whose walk re-arms the timers the last process left.
    fn start(world: &World, name: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            world.clock.begin();
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                TENANT,
                parse_csdl(CSDL).unwrap(),
                CSDL.to_string(),
                &[("Job", JOB)],
            );
            let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
            state.set_storage_stack(StorageStack::from_sim(world.events.clone(), None));
            state.state_timer_rows = Some(world.rows.clone());
            state
                .authz
                .reload_tenant_policies(TENANT, &world.cedar())
                .expect("the tenant's Cedar parses");
            let walk = state.start_runtime().await.expect("the server starts");
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

/// The withheld fires logged: the record and the setter each names.
#[derive(Clone, Default)]
struct Withheld(Arc<Mutex<Vec<(String, String)>>>);

impl Withheld {
    fn of(&self, id: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(record, _)| record == id)
            .map(|(_, setter)| setter.clone())
            .collect()
    }

    fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

/// The fields of a log line that name a withheld fire.
#[derive(Default)]
struct LineFields {
    message: String,
    entity_id: String,
    setter: String,
}

impl tracing::field::Visit for LineFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.set(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.set(
            field.name(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

impl LineFields {
    fn set(&mut self, name: &str, value: String) {
        match name {
            "message" => self.message = value,
            "entity_id" => self.entity_id = value,
            "setter" => self.setter = value,
            _ => {}
        }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Withheld {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = LineFields::default();
        event.record(&mut fields);
        if fields.message.starts_with("timer fire withheld") {
            self.0
                .lock()
                .unwrap()
                .push((fields.entity_id, fields.setter));
        }
    }
}

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

/// Let `by` pass. The paused clock jumps from one timer to the next.
async fn pass(by: Duration) {
    tokio::time::sleep(by).await;
    settle().await;
}

fn fail(seed: u64, trace: &[String], invariant: &str, detail: String) -> String {
    format!(
        "seed {seed}: {invariant}: {detail}\ntrace:\n  {}",
        trace.join("\n  ")
    )
}

/// One record: its user, its setter and its timer.
struct Record {
    id: String,
    user: String,
    setter: Setter,
    timer: Timer,
}

/// Check every record against the invariants, from its journal and rows
/// alone: a point read could rehydrate an actor and arm its timer again.
async fn check(
    seed: u64,
    trace: &[String],
    world: &World,
    state: &ServerState,
    records: &[Record],
    withheld: &Withheld,
    when: &str,
) -> Result<(), String> {
    let scheduled = world.events.list_scheduled_actions(TENANT).await.unwrap();
    let armed = state.armed_scheduled_actions();
    for record in records {
        let (_, fired, waits_in, moves_to) = record.timer.moves();
        let journal = world
            .events
            .read_events(&format!("{TENANT}:Job:{}", record.id), 0)
            .await
            .map_err(|error| fail(seed, trace, "journals are readable", error.to_string()))?;
        let moves = journal
            .iter()
            .filter(|event| event.event_type == fired)
            .count();
        let place = journal
            .last()
            .and_then(|event| event.payload.get("to_status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let row = world.rows.row(TENANT, &record.id);
        let scheduled_row = scheduled.iter().any(|row| row.entity_id == record.id);
        let live = armed.keys().any(|fire| fire.entity_id == record.id);
        let logged = withheld.of(&record.id);
        let detail = format!(
            "{when}: {} {:?} {:?}: {fired} made {moves} times, in {place}, state timer row {:?}, \
             scheduled row {scheduled_row}, live scheduled timer {live}, withheld as {logged:?}",
            record.id,
            record.setter,
            record.timer,
            row.as_ref().map(|row| (&row.state, &row.armed_by)),
        );
        if !record.setter.leaves() {
            if moves != 1 || place != moves_to {
                return Err(fail(
                    seed,
                    trace,
                    "a timer whose setter has standing, or that acts for no one, makes its move once",
                    detail,
                ));
            }
            continue;
        }
        if moves != 0 || place != waits_in {
            return Err(fail(
                seed,
                trace,
                "a timer whose setter has no standing never makes its move",
                detail,
            ));
        }
        match record.timer {
            Timer::State => {
                let as_setter = row.as_ref().is_some_and(|row| {
                    row.state == waits_in
                        && row
                            .armed_by
                            .contains(&format!("\"{}\"", record.setter.principal(&record.user)))
                        && row.armed_by.contains(&format!("\"{}\"", record.user))
                });
                if !as_setter {
                    return Err(fail(
                        seed,
                        trace,
                        "a withheld state timer keeps its row, armed as its setter",
                        detail,
                    ));
                }
            }
            Timer::Scheduled => {
                if scheduled_row || live {
                    return Err(fail(
                        seed,
                        trace,
                        "a withheld scheduled action leaves no row and no live timer",
                        detail,
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Check that each leaver's withheld fire was logged, naming its setter.
fn check_logged(
    seed: u64,
    trace: &[String],
    records: &[Record],
    withheld: &Withheld,
) -> Result<(), String> {
    for record in records {
        let logged = withheld.of(&record.id);
        let principal = record.setter.principal(&record.user);
        let named = !logged.is_empty() && logged.iter().all(|setter| *setter == principal);
        if record.setter.leaves() != named {
            return Err(fail(
                seed,
                trace,
                "only a fire withheld for its setter's standing is logged, naming the setter",
                format!("{} {:?}: logged {logged:?}", record.id, record.setter),
            ));
        }
    }
    Ok(())
}

/// One seed's run.
fn run_seed(seed: u64, withheld: &Withheld) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let mut rng = DeterministicRng::new(seed ^ 0x57a_d1e);
    let mut trace = Vec::new();
    withheld.clear();

    // Every setter with every timer, in a seeded order.
    let mut cases = Vec::new();
    for timer in [Timer::State, Timer::Scheduled] {
        for setter in [
            Setter::Stays(By::Themselves),
            Setter::Stays(By::Agent),
            Setter::Leaves(By::Themselves),
            Setter::Leaves(By::Agent),
            Setter::App,
            Setter::Internal,
        ] {
            cases.push((setter, timer));
        }
    }
    for i in (1..cases.len()).rev() {
        cases.swap(i, rng.next_bound(i + 1));
    }
    let records: Vec<Record> = cases
        .into_iter()
        .enumerate()
        .map(|(i, (setter, timer))| Record {
            id: format!("job-{i}"),
            user: format!("user-{i}"),
            setter,
            timer,
        })
        .collect();

    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
        people: Mutex::new(records.iter().map(|record| record.user.clone()).collect()),
    };
    let tenant = TenantId::new(TENANT);

    let process = Process::start(&world, &format!("standing-{seed}"));
    let set_going = process.runtime.block_on(async {
        for record in &records {
            // Every record is set going within a quarter of the wait.
            pass(Duration::from_secs(rng.next_bound(12) as u64)).await;
            process
                .state
                .get_or_create_tenant_entity(&tenant, "Job", &record.id, json!({}))
                .await
                .map_err(|error| fail(seed, &trace, "records are created", error.to_string()))?;
            let (sets, ..) = record.timer.moves();
            let set = process
                .state
                .dispatch_tenant_action(
                    &tenant,
                    "Job",
                    &record.id,
                    sets,
                    json!({}),
                    &record.setter.caller(&record.user),
                )
                .await
                .is_ok_and(|response| response.success);
            if !set {
                return Err(fail(
                    seed,
                    &trace,
                    "records are set going",
                    record.id.clone(),
                ));
            }
            trace.push(format!(
                "{}: {:?} {sets} at {}",
                record.id,
                record.setter,
                sim_now()
            ));
        }
        settle().await;
        Ok(())
    });
    if let Err(report) = set_going {
        process.kill(&world, Duration::ZERO);
        return Err(report);
    }

    // The leavers leave, well before any timer is due.
    {
        let mut people = world.people.lock().unwrap();
        for record in records.iter().filter(|record| record.setter.leaves()) {
            people.remove(&record.user);
        }
    }
    process
        .state
        .authz
        .reload_tenant_policies(TENANT, &world.cedar())
        .expect("the tenant's Cedar parses");
    trace.push(format!("leavers left at {}", sim_now()));

    // Restart before the timers fire, or while they fall due, or not at all.
    let process = match seed % 3 {
        0 => process,
        restart => {
            let downtime = if restart == 1 {
                Duration::ZERO
            } else {
                WAIT * 2
            };
            trace.push(format!("restart, down {}s", downtime.as_secs()));
            process.kill(&world, downtime);
            Process::start(&world, &format!("standing-{seed}-back"))
        }
    };
    let after_fires = process.runtime.block_on(async {
        pass(WAIT + HORIZON).await;
        check(
            seed,
            &trace,
            &world,
            &process.state,
            &records,
            withheld,
            "after the fires",
        )
        .await
    });
    process.kill(&world, Duration::ZERO);
    after_fires?;
    check_logged(seed, &trace, &records, withheld)?;

    // Every later boot arms the kept rows again, as their setters.
    trace.push("restart after the fires".to_string());
    let process = Process::start(&world, &format!("standing-{seed}-again"));
    let after_restart = process.runtime.block_on(async {
        pass(HORIZON).await;
        check(
            seed,
            &trace,
            &world,
            &process.state,
            &records,
            withheld,
            "after the last restart",
        )
        .await
    });
    process.kill(&world, Duration::ZERO);
    after_restart
}

#[test]
fn dst_a_timer_acts_with_its_setters_current_standing() {
    let withheld = Withheld::default();
    let _logs =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(withheld.clone()));
    // Seeds that broke each invariant, and the first such seed's report.
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed, &withheld) {
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
