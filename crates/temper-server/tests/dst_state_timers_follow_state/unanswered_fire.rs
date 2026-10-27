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

/// A failed replacement write must leave a bootable row, even if deletion
/// would succeed. No record read or external action rescues it after boot.
#[test]
fn failed_retry_write_survives_restart() {
    for seed in [99, 100, 101] {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let spec = expire_admits_one();
        let (world, process, job) = started(seed, clock, &spec, "failed-retry-write");
        process.runtime.block_on(async {
            let first = world.rows.row("gamma", &job.id).unwrap();
            let _in_flight = hold_expire_permit(&process.state, &job.tenant).await;
            world.rows.fail_next_writes(1);
            advance(PAST_THE_BUDGET).await;
            assert_eq!(
                world
                    .rows
                    .row("gamma", &job.id)
                    .expect("failed replacement retains the old durable arm")
                    .arm_id,
                first.arm_id
            );
        });
        process.kill(&world, Duration::ZERO);
        let restarted = Process::start(&world, JOB_V1, &spec, "retry-after-restart");
        restarted.runtime.block_on(async {
            advance(Duration::from_secs(1)).await;
            assert_eq!(status(&restarted.state, &job).await, "Done");
            assert!(world.rows.row("gamma", &job.id).is_none());
        });
        restarted.kill(&world, Duration::ZERO);
    }
}

/// Even across consecutive failed saves, a later answer (including a guard
/// refusal) removes the retained row, so boot cannot resurrect an ended fire.
#[test]
fn answered_retry_removes_retained_row() {
    for refused in [false, true] {
        let seed = 102;
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let spec = if refused {
            expire_refused()
                + "\n[admission]\nmax_concurrent_actions = { \"Expire\" = 1 }\nqueue_timeout_seconds = 0\n"
        } else {
            expire_admits_one()
        };
        let (world, process, job) = started(seed, clock, &spec, "answered-retry");
        process.runtime.block_on(async {
            let in_flight = hold_expire_permit(&process.state, &job.tenant).await;
            world.rows.fail_next_writes(2);
            advance(PAST_THE_BUDGET).await;
            advance(PAST_THE_BUDGET).await;
            assert!(world.rows.row("gamma", &job.id).is_some());
            drop(in_flight);
            advance(PAST_THE_BUDGET).await;
            assert_eq!(
                status(&process.state, &job).await,
                if refused { "Waiting" } else { "Done" }
            );
            assert!(
                world.rows.row("gamma", &job.id).is_none(),
                "an answer ends the durable timer"
            );
        });
        process.kill(&world, Duration::ZERO);
    }
}

/// B replaces A but reports failure; C fails before saving. An answer from C
/// must remove B as well as A, so a restart cannot resurrect an ended timer.
#[test]
fn answered_retry_removes_a_row_saved_despite_an_error() {
    for seed in [103, 104, 105] {
        for refused in [true, false] {
            let clock = Arc::new(ProcessClock::new());
            let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
            let spec = if refused {
                expire_refused()
                    + "\n[admission]\nmax_concurrent_actions = { \"Expire\" = 1 }\nqueue_timeout_seconds = 0\n"
            } else {
                expire_admits_one()
            };
            let (world, process, job) = started(seed, clock, &spec, "ambiguous-retry-write");
            process.runtime.block_on(async {
                let first = world.rows.row("gamma", &job.id).unwrap();
                let in_flight = hold_expire_permit(&process.state, &job.tenant).await;
                world.rows.fail_after_next_writes(1);
                advance(PAST_THE_BUDGET).await;
                let saved_retry = world.rows.row("gamma", &job.id).unwrap();
                assert_ne!(saved_retry.arm_id, first.arm_id, "B landed despite its error");

                world.rows.fail_next_writes(1);
                advance(PAST_THE_BUDGET).await;
                assert_eq!(
                    world.rows.row("gamma", &job.id).unwrap().arm_id,
                    saved_retry.arm_id,
                    "C failed before saving, so B is still durable"
                );
                drop(in_flight);
                advance(PAST_THE_BUDGET).await;
                assert_eq!(
                    status(&process.state, &job).await,
                    if refused { "Waiting" } else { "Done" }
                );
                assert!(
                    world.rows.row("gamma", &job.id).is_none(),
                    "an answer must end even a timer whose save reported an error: seed={seed}, refused={refused}"
                );
            });
            process.kill(&world, Duration::ZERO);
            let restarted = Process::start(&world, JOB_V1, &spec, "ended-after-ambiguous-write");
            assert_eq!(
                restarted.booted[&(job.tenant.clone(), "Job".to_string())].rearmed,
                0,
                "boot must not re-arm an answered timer"
            );
            restarted.runtime.block_on(async {
                advance(PAST_THE_BUDGET).await;
                assert_eq!(
                    status(&restarted.state, &job).await,
                    if refused { "Waiting" } else { "Done" }
                );
                assert!(world.rows.row("gamma", &job.id).is_none());
            });
            restarted.kill(&world, Duration::ZERO);
        }
    }
}
