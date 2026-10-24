//! Regression: a timer whose fire gets no answer fires again a budget later;
//! one whose fire is refused does not.
//!
//! On 2026-10-24 a record's repeating timer stopped for about 20 minutes
//! around an app install: its fire's dispatch failed before the record
//! answered, the timer removed its row, and nothing armed it again until
//! someone dispatched its action by hand. Here the fire's dispatch is deferred
//! by admission, one of the ways a dispatch fails without an answer: the one
//! permit `Expire` admits is held, as by a dispatch still in flight.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::state::{AdmissionOutcome, AdmissionPermit};
use temper_store_sim::SimEventStore;

use super::world::{ProcessClock, SimTimerRows};
use super::{JOB_V1, Job, Process, World, act, advance, settle, status};

/// `Waiting`'s budget in `JOB_V1`, and a second more.
const PAST_THE_BUDGET: Duration = Duration::from_secs(3601);

/// `JOB_V1` where `Expire` admits one dispatch at a time and defers any other
/// at once.
fn expire_admits_one() -> String {
    format!(
        "{JOB_V1}\n[admission]\nmax_concurrent_actions = {{ \"Expire\" = 1 }}\nqueue_timeout_seconds = 0\n"
    )
}

/// `JOB_V1` where `Expire` needs `ready`, which nothing sets: its guard refuses
/// every fire of `Waiting`'s timer.
fn expire_refused() -> String {
    let expire = "name = \"Expire\"\nfrom = [\"Waiting\", \"Review\", \"Done\"]\nto = \"Done\"";
    assert!(
        JOB_V1.contains(expire),
        "JOB_V1 declares Expire as expected"
    );
    JOB_V1
        .replacen(expire, &format!("{expire}\nguard = \"ready\""), 1)
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"ready\"\ntype = \"bool\"\ninitial = false\n\n[[action]]",
            1,
        )
}

/// One gamma record, created and started, under a process whose gamma `Job`
/// is `gamma_spec`; the timer row its `Start` wrote has landed. `clock` is the
/// simulation clock the test installed.
fn started(
    seed: u64,
    clock: Arc<ProcessClock>,
    gamma_spec: &str,
    name: &str,
) -> (World, Process, Job) {
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let job = Job {
        tenant: TenantId::new("gamma"),
        id: name.to_string(),
    };
    let process = Process::start(&world, JOB_V1, gamma_spec, name);
    process.runtime.block_on(async {
        process
            .state
            .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
            .await
            .unwrap();
        assert!(act(&process.state, &job, "Start").await);
        settle().await;
    });
    (world, process, job)
}

/// Hold the one permit `tenant`'s `Expire` admits, as a dispatch in flight
/// does.
async fn hold_expire_permit(state: &ServerState, tenant: &TenantId) -> AdmissionPermit {
    let admission = state
        .registry
        .read()
        .unwrap()
        .get_spec(tenant, "Job")
        .and_then(|spec| spec.automaton.admission.clone())
        .expect("the Job spec declares admission");
    match state
        .admission
        .try_acquire_with_caps(tenant, "Job", "Expire", Some(&admission))
        .await
    {
        AdmissionOutcome::Granted(permit) => permit,
        _ => panic!("the one Expire permit is free"),
    }
}

/// Without a dispatch or a restart, the timer whose fire got no answer is
/// armed again, keeps a row a restart would arm, and finishes the record.
#[test]
fn an_unanswered_fire_fires_again_a_budget_later() {
    let seed = 97;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let (world, process, job) = started(seed, clock, &expire_admits_one(), "unanswered");
    process.runtime.block_on(async {
        let state = &process.state;
        let first = world
            .rows
            .row("gamma", &job.id)
            .expect("Start arms Waiting's timer");

        let in_flight = hold_expire_permit(state, &job.tenant).await;
        advance(PAST_THE_BUDGET).await;
        assert_eq!(
            status(state, &job).await,
            "Waiting",
            "the deferred fire did not run"
        );
        let again = world
            .rows
            .row("gamma", &job.id)
            .expect("a fire that got no answer leaves its record a timer");
        assert_ne!(again.arm_id, first.arm_id, "the timer is armed again");
        assert_eq!(again.state, "Waiting");

        // Nobody acts on the record: the timer alone finishes it.
        drop(in_flight);
        advance(PAST_THE_BUDGET).await;
        assert_eq!(status(state, &job).await, "Done");
        assert!(
            world.rows.row("gamma", &job.id).is_none(),
            "the answered timer ended"
        );
    });
    process.kill(&world, Duration::ZERO);
}

/// A guard's no is an answer: the timer ends and is not armed again, so a
/// record a guard holds is not polled every budget.
#[test]
fn a_refused_fire_is_not_armed_again() {
    let seed = 98;
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let (world, process, job) = started(seed, clock, &expire_refused(), "refused");
    process.runtime.block_on(async {
        let state = &process.state;
        assert!(
            world.rows.row("gamma", &job.id).is_some(),
            "Start arms Waiting's timer"
        );
        advance(PAST_THE_BUDGET).await;
        assert_eq!(status(state, &job).await, "Waiting", "the guard refused");
        assert!(
            world.rows.row("gamma", &job.id).is_none(),
            "a refused fire is not armed again"
        );
    });
    process.kill(&world, Duration::ZERO);
}
