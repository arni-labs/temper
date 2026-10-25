//! DST: a sandbox that sleeps when idle is woken in time for every timer.
//!
//! A user's Temper runs in a sandbox that is suspended a while after its last
//! inbound request and resumed by the next one. While it is suspended the wall
//! clock moves on and the process's monotonic clock, which its timers wait on,
//! stands still. The process reports the earliest due time it could sleep
//! through, the control plane wakes it then, and a wake is a request like any
//! other.
//!
//! Workload, per seed (`sandbox.rs`): requests arrive over eight hours, some
//! close together and some hours apart. Each makes a record that arms a
//! `schedule_at` action (`Fire`), enters a state with an hour's timeout
//! (`Expire`) or starts a chain repeating every two hours (`Tick`), or only
//! keeps the sandbox awake. The sandbox is suspended exactly 900 s plus up to
//! a minute after the last inbound request; the process is told it stays awake
//! 600 s. The control plane wakes it at the due time it was told last, by a
//! report or a wake's answer, sometimes twice at once, and fails up to three
//! report sends in a row.
//!
//! Invariants (`checks.rs`):
//! - every timer fires at most 10 s after it is due, and none is slept
//!   through;
//! - nothing fires twice;
//! - when a wake answers, nothing of its tenant due by then waits, and it
//!   names the earliest due time at or after the horizon; a wake right after
//!   another fires nothing;
//! - told a window longer than the sandbox really stays awake, the process
//!   sleeps through a timer, and the checks see it;
//! - a request that resumes the sandbox makes its timers wait again by the
//!   wall clock, so one armed before the suspension fires on time;
//! - a chain firing every minute costs the control plane about one report and
//!   one wake per window, idle or busy, not one per fire.

use std::time::Duration;

use serde_json::json;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::request_context::AgentContext;

#[path = "dst_runtime_wake/checks.rs"]
mod checks;
#[path = "dst_runtime_wake/refused.rs"]
mod refused;
#[path = "dst_runtime_wake/sandbox.rs"]
mod sandbox;
#[path = "dst_runtime_wake/world.rs"]
mod world;

use sandbox::{Kind, Setup};

const TENANT: &str = "alpha";
/// The `Waiting` state's timeout.
const TIMEOUT: i64 = 3600;
/// How far apart `Tick`s and `Pulse`s are.
const TICK: i64 = 7200;
const PULSE: i64 = 60;
/// How long the sandbox really stays awake after an inbound request, before
/// up to [`JITTER`] more.
const SUSPEND_AFTER: Duration = Duration::from_secs(900);
const JITTER: u64 = 60;
/// How long the process is told it stays awake: below [`SUSPEND_AFTER`].
const AWAKE: Duration = Duration::from_secs(600);
/// The most a timer may fire after it is due: a report's delivery through
/// three failed sends (1 + 2 + 4 s), and then some.
const LATE: i64 = 10;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Planned", "Waiting", "Active", "Humming", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Planned", "Active", "Humming", "Done"]

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
effect = ["schedule('Tick', 7200)"]

[[action]]
name = "Tick"
from = ["Active"]
effect = ["schedule('Tick', 7200)"]

[[action]]
name = "Hum"
from = ["Idle"]
to = "Humming"
effect = ["schedule('Pulse', 60)"]

[[action]]
name = "Pulse"
from = ["Humming"]
effect = ["schedule('Pulse', 60)"]

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
<Action Name="Hum" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Pulse" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
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

/// Every seed's failure, if any, and how many reports in all landed after
/// the answer of a wake made after them.
fn failures(seeds: u64, setup: impl Fn() -> Setup) -> (Vec<String>, u64) {
    let mut failures = Vec::new();
    let mut overtaken = 0;
    for seed in 0..seeds {
        match sandbox::run(seed, setup()) {
            Ok(n) => overtaken += n,
            Err(failure) => failures.push(failure),
        }
    }
    (failures, overtaken)
}

#[test]
fn dst_a_sleeping_sandbox_is_woken_for_every_timer() {
    let (failures, overtaken) = failures(48, || Setup {
        awake: Some(AWAKE),
        kind: Kind::Mixed,
    });
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    assert!(
        overtaken > 0,
        "no report sent before a wake landed after its answer"
    );
}

/// The checks can see a wrong window: told it stays awake longer than the
/// sandbox really does, the process leaves a timer unreported that falls due
/// after the suspension, and it is slept through.
#[test]
fn dst_a_window_longer_than_the_sandbox_stays_awake_is_caught() {
    let (failures, _) = failures(48, || Setup {
        awake: Some(SUSPEND_AFTER * 2),
        kind: Kind::Mixed,
    });
    assert!(
        failures.iter().any(|f| f.contains(checks::ON_TIME)),
        "a window longer than the sandbox stays awake went unseen: {failures:?}"
    );
}

/// A request resumes the sandbox shortly before two timers armed before its
/// suspension: both fire on time.
#[test]
fn dst_a_request_that_resumes_the_sandbox_retimes_its_timers() {
    let (failures, _) = failures(8, || Setup {
        awake: Some(AWAKE),
        kind: Kind::Resumed,
    });
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// A chain firing every minute keeps the sandbox awake through the control
/// plane's wakes, and costs it about one report and one wake per window, not
/// one per fire.
#[test]
fn dst_a_minute_chain_reports_about_once_a_window() {
    for busy in [false, true] {
        let (failures, _) = failures(16, || Setup {
            awake: Some(AWAKE),
            kind: Kind::Chain { busy },
        });
        assert!(
            failures.is_empty(),
            "busy {busy}: {}",
            failures.join("\n\n")
        );
    }
}

/// Make the record `id`.
async fn job(state: &ServerState, id: &str) {
    state
        .get_or_create_tenant_entity(&TenantId::new(TENANT), "Job", id, json!({}))
        .await
        .unwrap();
}
