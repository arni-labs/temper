//! DST: a suspended process woken by wall clock fires what fell due, once.
//!
//! A user's Temper sleeps when its sandbox is suspended, and its timers wait
//! on the process's monotonic clock, which may not advance while it sleeps.
//! The control plane wakes it at the earliest due time the process reported.
//!
//! Workload, per seed: records arm a `schedule_at` action (`Fire`), enter a
//! state with a timeout (`Expire` after an hour), or start a repeating action
//! (`Tick` every ten minutes). Time passes normally for a while, then the
//! process is suspended several times: the wall clock moves on, past some due
//! times, while the process's timers stand still. Each suspension ends with a
//! wake (`ServerState::wake_runtime`), sometimes a second one at once. While
//! the process is awake, requests keep arming records due soon, and the
//! control plane wakes it only at the earliest due time it was told. The
//! control plane's end of the reports refuses up to three sends in a row.
//!
//! Invariants:
//! - when a wake answers, every row that was due before it has fired, exactly
//!   once, and nothing that was not due has;
//! - the wake's answer, and the report the process sent last, name the
//!   earliest due time of the rows left;
//! - a wake right after another fires nothing;
//! - while the process is awake, its timers late by the time it was
//!   suspended, the control plane is told of every row that waits, and every
//!   row fires at a wake for the time it was told, late by no more than a
//!   report's delivery through the refusals;
//! - when the old waits run out, nothing fires twice.

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
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

#[path = "dst_runtime_wake/world.rs"]
mod world;

use world::{Clock, MOST_REFUSED, Reports, TimerRows};

const SEEDS: u64 = 48;
const TENANT: &str = "alpha";
/// The `Waiting` state's timeout, and how far apart `Tick`s are.
const TIMEOUT: i64 = 3600;
const PERIOD: i64 = 600;
/// How long a report takes to get through the most refusals in a row: its
/// tries wait 1 s, then 2 s, then 4 s, doubling.
const DELIVERY: Duration = Duration::from_secs((1 << MOST_REFUSED) + 1);

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Planned", "Waiting", "Active", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Planned", "Active", "Done"]

[[action]]
name = "Plan"
from = ["Idle"]
to = "Planned"
params = ["due"]
effect = ["schedule_at('Fire', due)"]

[[action]]
name = "Fire"
from = ["Planned", "Done"]
to = "Done"

[[action]]
name = "Wait"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Expire"
from = ["Waiting", "Done"]
to = "Done"

[[action]]
name = "Activate"
from = ["Idle"]
to = "Active"
effect = ["schedule('Tick', 600)"]

[[action]]
name = "Tick"
from = ["Active"]
effect = ["schedule('Tick', 600)"]

[[state_timeout]]
state = "Waiting"
after_seconds = 3600
on_timeout = "Expire"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="due" Type="Edm.String"/>
</EntityType>
<Action Name="Plan" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/><Parameter Name="due" Type="Edm.String"/></Action>
<Action Name="Fire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Wait" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Expire" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Activate" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Tick" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
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

/// How many of the record's events are `action`.
async fn count(events: &SimEventStore, id: &str, action: &str) -> usize {
    events
        .read_events(&format!("{TENANT}:Job:{id}"), 0)
        .await
        .map(|journal| journal.iter().filter(|e| e.event_type == action).count())
        .unwrap_or_default()
}

/// One waiting row: whose, which action, and when it is due.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Due {
    at: DateTime<Utc>,
    id: String,
    action: String,
}

/// Every row waiting now, from the store: the rows are what is due.
async fn waiting(events: &SimEventStore, rows: &TimerRows) -> Vec<Due> {
    let mut due = Vec::new();
    for row in events.list_scheduled_actions(TENANT).await.unwrap() {
        due.push(Due {
            at: DateTime::parse_from_rfc3339(&row.due_at).unwrap().to_utc(),
            id: row.entity_id,
            action: row.action,
        });
    }
    for row in rows.list_state_timers(TENANT).await.unwrap() {
        let started = DateTime::parse_from_rfc3339(&row.started_at).unwrap();
        due.push(Due {
            at: started.to_utc() + chrono::Duration::seconds(TIMEOUT),
            id: row.entity_id,
            action: "Expire".to_string(),
        });
    }
    due.sort();
    due
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

/// The fires each record has had so far, by action.
async fn fires(events: &SimEventStore, ids: &[String]) -> BTreeMap<(String, String), usize> {
    let mut out = BTreeMap::new();
    for id in ids {
        for action in ["Fire", "Expire", "Tick"] {
            out.insert(
                (id.clone(), action.to_string()),
                count(events, id, action).await,
            );
        }
    }
    out
}

/// Check one wake: what was due fired once, nothing else did, and the
/// answer and the last report name the earliest row left.
async fn check_wake(
    state: &ServerState,
    events: &SimEventStore,
    rows: &TimerRows,
    reports: &Reports,
    ids: &[String],
    run: &Run,
    late_by: Option<i64>,
) -> Result<usize, String> {
    let before = waiting(events, rows).await;
    let fired_before = fires(events, ids).await;
    let woke_at = sim_now();
    let sent_before = reports.sent.lock().unwrap().len();
    let outcome = state.wake_runtime(&TenantId::new(TENANT)).await;
    let answered_at = sim_now();
    let due: Vec<&Due> = before.iter().filter(|d| d.at <= woke_at).collect();
    if let Some(late_by) = late_by
        && let Some(late) = due
            .iter()
            .find(|d| (woke_at - d.at).num_seconds() > late_by)
    {
        return Err(run.fail(
            "the control plane, woken at the time it was told, wakes each row when it is due",
            format!("{late:?} woken at {woke_at}, more than {late_by}s late"),
        ));
    }
    let fired_after = fires(events, ids).await;
    for ((id, action), after) in &fired_after {
        let was = fired_before[&(id.clone(), action.clone())];
        let expected = due
            .iter()
            .filter(|d| &d.id == id && &d.action == action)
            .count();
        if *after != was + expected {
            return Err(run.fail(
                "a wake fires every row due before it, exactly once, and nothing else",
                format!(
                    "{id} {action}: {was} fires before the wake at {woke_at}, {after} after, \
                     {expected} due; rows {before:?}"
                ),
            ));
        }
    }
    if outcome.fired != due.len() {
        return Err(run.fail(
            "a wake answers how many rows it fired",
            format!("answered {}, {} were due", outcome.fired, due.len()),
        ));
    }
    let left = waiting(events, rows).await;
    let earliest = left.first().map(|d| d.at);
    if outcome.next_due_at != earliest {
        return Err(run.fail(
            "a wake answers the earliest due time left",
            format!("answered {:?}, rows left {left:?}", outcome.next_due_at),
        ));
    }
    settle().await;
    let sent = reports.sent.lock().unwrap();
    let last = sent[sent_before..].last().cloned();
    drop(sent);
    match last {
        Some(report) if report.next_due_at == earliest && report.reported_at >= woke_at => {}
        other => {
            return Err(run.fail(
                "the process reports the earliest due time left after a wake",
                format!("last report {other:?}, earliest {earliest:?}, answered at {answered_at}"),
            ));
        }
    }
    Ok(due.len())
}

/// What the process and its control plane share in a run.
struct World<'a> {
    state: &'a ServerState,
    events: &'a SimEventStore,
    rows: &'a TimerRows,
    reports: &'a Reports,
}

/// The process awake, up to `until`, or with none until only repeating
/// actions are left. Requests sometimes arm a record due sooner than anything
/// else may be, which only its own report tells the control plane, and the
/// control plane wakes the process only at the earliest due time it was
/// delivered. Ok: how many rows those wakes fired.
async fn awake(
    w: &World<'_>,
    ids: &mut Vec<String>,
    run: &mut Run,
    rng: &mut DeterministicRng,
    until: Option<DateTime<Utc>>,
) -> Result<usize, String> {
    let late_by = DELIVERY.as_secs() as i64 + 1;
    let mut woken = 0;
    for round in 0.. {
        let arrived = ids.iter().filter(|id| id.contains("-late-")).count();
        if arrived < 12 && rng.next_bound(3) == 0 {
            let id = format!("job-{}-late-{arrived}", run.seed);
            w.state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", &id, json!({}))
                .await
                .unwrap();
            let due = sim_now() + chrono::Duration::seconds(30 + rng.next_bound(600) as i64);
            if !act(w.state, &id, "Plan", json!({ "due": due.to_rfc3339() })).await {
                return Err(run.fail("the workload arms", format!("{id} Plan refused")));
            }
            run.trace
                .push(format!("{id} Plan at {}, due {due}", sim_now()));
            ids.push(id);
        }
        // Long enough for a report to get through the refusals.
        let rest = until.map_or(DELIVERY, |u| {
            (u - sim_now()).to_std().unwrap_or_default().min(DELIVERY)
        });
        tokio::time::sleep(rest).await;
        settle().await;
        let left = waiting(w.events, w.rows).await;
        if until.is_some_and(|u| sim_now() >= u)
            || until.is_none() && left.iter().all(|d| d.action == "Tick")
        {
            break;
        }
        if round == 1000 {
            return Err(run.fail("one-shot rows run out", format!("{left:?}")));
        }
        if left.is_empty() {
            continue;
        }
        let Some(at) = w.reports.told().and_then(|told| told.next_due_at) else {
            return Err(run.fail(
                "the control plane is told of every row that waits",
                format!("it was told nothing is due; rows {left:?}"),
            ));
        };
        if let Some(u) = until
            && at > u
        {
            tokio::time::sleep((u - sim_now()).to_std().unwrap_or_default()).await;
            settle().await;
            break;
        }
        if at > sim_now() {
            tokio::time::sleep((at - sim_now()).to_std().unwrap()).await;
        }
        settle().await;
        run.trace
            .push(format!("woken at the time it was told {}", sim_now()));
        woken += check_wake(
            w.state,
            w.events,
            w.rows,
            w.reports,
            ids,
            run,
            Some(late_by),
        )
        .await?;
    }
    Ok(woken)
}

/// One seed's run. Ok: how many rows its wakes fired.
fn run_seed(seed: u64) -> Result<usize, String> {
    let clock = Arc::new(Clock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let mut rng = DeterministicRng::new(seed);
    let events = SimEventStore::no_faults(seed);
    let rows = Arc::new(TimerRows::default());
    let reports = Arc::new(Reports::new(seed));
    let mut run = Run {
        seed,
        trace: Vec::new(),
    };
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
        let mut state = ServerState::from_registry(ActorSystem::new("wake"), registry);
        state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
        state.state_timer_rows = Some(rows.clone());
        state.set_wake_sink(reports.clone());
        state
            .start_runtime()
            .await
            .expect("the first start")
            .await
            .unwrap();

        let mut ids = Vec::new();
        for i in 0..3 + rng.next_bound(6) {
            let id = format!("job-{seed}-{i}");
            state
                .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", &id, json!({}))
                .await
                .unwrap();
            let (action, params) = match rng.next_bound(3) {
                0 => {
                    let due = sim_now() + chrono::Duration::seconds(rng.next_bound(7200) as i64);
                    ("Plan", json!({ "due": due.to_rfc3339() }))
                }
                1 => ("Wait", json!({})),
                _ => ("Activate", json!({})),
            };
            if !act(&state, &id, action, params).await {
                return Err(run.fail("the workload arms", format!("{id} {action} refused")));
            }
            run.trace.push(format!("{id} {action} at {}", sim_now()));
            ids.push(id);
            tokio::time::sleep(Duration::from_secs(rng.next_bound(300) as u64)).await;
            settle().await;
        }
        let world = World {
            state: &state,
            events: &events,
            rows: &rows,
            reports: &reports,
        };
        let mut woken = 0;
        let mut suspended = Duration::ZERO;
        for _ in 0..1 + rng.next_bound(4) {
            let by = Duration::from_secs(rng.next_bound(5400) as u64);
            clock.suspend(by);
            suspended += by;
            run.trace.push(format!(
                "suspended {}s, wake at {}",
                by.as_secs(),
                sim_now()
            ));
            woken += check_wake(&state, &events, &rows, &reports, &ids, &run, None).await?;
            if rng.next_bound(2) == 0 {
                run.trace.push(format!("wake again at {}", sim_now()));
                if check_wake(&state, &events, &rows, &reports, &ids, &run, None).await? != 0 {
                    return Err(run.fail("a wake right after another fires nothing", String::new()));
                }
            }
            let until = sim_now() + chrono::Duration::seconds(rng.next_bound(900) as i64);
            woken += awake(&world, &mut ids, &mut run, &mut rng, Some(until)).await?;
        }

        // The process runs on, its timers late by the time it was suspended,
        // until only repeating actions are left.
        woken += awake(&world, &mut ids, &mut run, &mut rng, None).await?;
        // When the old waits of the timers the wakes fired run out, nothing
        // fires again: every record fired its one-shot action at most once.
        tokio::time::sleep(suspended + Duration::from_secs((TIMEOUT + PERIOD) as u64)).await;
        settle().await;
        for ((id, action), fired) in fires(&events, &ids).await {
            if action != "Tick" && fired > 1 {
                return Err(run.fail(
                    "nothing fires twice",
                    format!("{id} {action} fired {fired} times"),
                ));
            }
        }
        Ok(woken)
    });
    drop(runtime);
    result
}

#[test]
fn dst_a_woken_process_fires_what_fell_due_once() {
    let mut failures = Vec::new();
    let mut woken = 0;
    for seed in 0..SEEDS {
        match run_seed(seed) {
            Ok(fired) => woken += fired,
            Err(failure) => failures.push(failure),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert!(
        woken > 0,
        "no wake fired anything: the suspensions never passed a due time"
    );
}
