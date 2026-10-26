//! DST: the timers a boot finds due fire a few at a time, including those of
//! records that have no timer row.
//!
//! Workload, per seed: records in `alpha` start under the first spec and are
//! never passivated, so none has a snapshot; records in `gamma` start untimed.
//! Each record is started by its own caller. The process stays down longer
//! than any budget, then boots under the second spec with `gamma` timed.
//! `alpha`'s records replay into their stored `Waiting`, whose new budget
//! replaces the timer their rows name. `gamma`'s records have no row: they
//! entered `Waiting` before their type had a timeout, as records that entered
//! a state before rows were kept, or whose arming was lost. Every timer is due
//! at boot. Right after the boot, while the fires queue for the boot's gate,
//! some `gamma` records are moved out of `Waiting` by another caller. A
//! record's first append in the new process takes a minute, so fires that run
//! together start at the same instant.
//!
//! Invariants:
//! - the boot leaves no record in a timed state waiting: none of these
//!   timeout actions binds its caller;
//! - at most [`FIRES_AT_ONCE`] timers start firing at one instant;
//! - a timer fires only while its record is in its state, so a record moved
//!   out of `Waiting` before its fire is not fired after the move;
//! - every record that is not moved reaches `Done` on exactly one fire, and no
//!   record fires twice;
//! - a fire runs as whoever armed the timer: an `alpha` record's as its
//!   starter, whose row keeps them; a `gamma` record's, which has no row and
//!   whose journal does not keep who entered the state, as the kernel's
//!   state-timer service.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use temper_store_sim::SimEventStore;

use super::world::{ProcessClock, SimTimerRows};
use super::{JOB_V1, Job, Process, Run, World, act, advance, check_seeds, job_untimed, job_v2};

/// How many due timers the server lets fire at once (`BOOT_FIRES_AT_ONCE`).
const FIRES_AT_ONCE: usize = 8;

/// How long a record's first append in the restarted process takes.
const FIRE_APPEND: Duration = Duration::from_secs(60);

/// Who a timer fires as when nobody armed it (`STATE_TIMER_SERVICE`).
const STATE_TIMER_SERVICE: &str = "service:state-timer";

#[test]
fn dst_due_timers_fire_a_few_at_a_time() {
    check_seeds(run_seed);
}

/// A caller named `agent_id`.
fn caller(agent_id: &str) -> AgentContext {
    AgentContext {
        agent_id: Some(agent_id.to_string()),
        ..AgentContext::default()
    }
}

/// Who starts `job`.
fn starter(job: &Job) -> String {
    format!("starter-{}-{}", job.tenant, job.id)
}

fn run_seed(seed: u64) -> Result<(), String> {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
    let world = World {
        events: SimEventStore::no_faults(seed),
        rows: Arc::new(SimTimerRows::new(seed)),
        clock,
    };
    let mut rng = DeterministicRng::new(seed ^ 0xb0b5);
    let mut run = Run::new(seed);
    let mut jobs = Vec::new();
    for tenant in ["alpha", "gamma"] {
        for i in 0..FIRES_AT_ONCE + 1 + rng.next_bound(4) {
            jobs.push(Job {
                tenant: TenantId::new(tenant),
                id: format!("job-{i}"),
            });
        }
    }

    let first = Process::start(&world, JOB_V1, &job_untimed(), &format!("burst-{seed}-a"));
    first.runtime.block_on(async {
        let state = &first.state;
        for job in &jobs {
            advance(Duration::from_secs(rng.next_bound(60) as u64)).await;
            state
                .get_or_create_tenant_entity(&job.tenant, "Job", &job.id, json!({}))
                .await
                .map_err(|error| run.fail("records are created", error))?;
            let started = state
                .dispatch_tenant_action(
                    &job.tenant,
                    "Job",
                    &job.id,
                    "Start",
                    json!({}),
                    &caller(&starter(job)),
                )
                .await
                .is_ok_and(|response| response.success);
            if !started {
                return Err(run.fail("records start", format!("{}/{}", job.tenant, job.id)));
            }
        }
        Ok(())
    })?;
    // Longer than either budget: every timer is due when the server is back.
    let downtime = Duration::from_secs(3601 + rng.next_bound(3600) as u64);
    run.note(format!(
        "started {} records; restart after {}s down",
        jobs.len(),
        downtime.as_secs()
    ));
    first.kill(&world, downtime);
    for job in &jobs {
        world
            .events
            .inject_append_delay(&format!("{}:Job:{}", job.tenant, job.id), FIRE_APPEND);
    }

    let mut second = Process::start(&world, &job_v2(), JOB_V1, &format!("burst-{seed}-b"));
    // The moves since before the boot armed any timer; the process keeps a
    // fresh subscription it does not read.
    let mut changes = std::mem::replace(&mut second.changes, second.state.event_tx.subscribe());
    second.runtime.block_on(async {
        let state = &second.state;
        let moved: BTreeSet<String> = jobs
            .iter()
            .filter(|job| job.tenant.as_str() == "gamma" && rng.chance(0.3))
            .map(|job| job.id.clone())
            .collect();
        run.note(format!(
            "moved gamma {moved:?} out of Waiting after the boot"
        ));
        let moves: Vec<_> = moved
            .iter()
            .map(|id| {
                let (state, id) = (state.clone(), id.clone());
                // determinism-ok: the moves run on the test's single-threaded runtime.
                tokio::spawn(async move {
                    let job = Job {
                        tenant: TenantId::new("gamma"),
                        id,
                    };
                    let moved = act(&state, &job, "Approve").await;
                    (job.id, moved)
                })
            })
            .collect();
        for _ in 0..4 * jobs.len() / FIRES_AT_ONCE + 4 {
            advance(FIRE_APPEND).await;
        }
        for handle in moves {
            let (id, accepted) = handle.await.unwrap();
            if !accepted {
                return Err(run.fail("moves are accepted", format!("gamma/{id}")));
            }
        }
        let mut fired_as: BTreeMap<(String, String), Vec<Option<String>>> = BTreeMap::new();
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        loop {
            match changes.try_recv() {
                Ok(change) => {
                    *seen.entry(change.action.clone()).or_default() += 1;
                    if change.action == "Expire" {
                        fired_as
                            .entry((change.tenant.clone(), change.entity_id.clone()))
                            .or_default()
                            .push(change.agent_id.clone());
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(missed)) => {
                    return Err(run.fail(
                        "the simulation sees every move",
                        format!("{missed} moves were not seen"),
                    ));
                }
                Err(_) => break,
            }
        }
        let mut fired_at: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for job in &jobs {
            let name = format!("{}/{}", job.tenant, job.id);
            let record = state
                .get_tenant_entity_state(&job.tenant, "Job", &job.id)
                .await
                .map_err(|error| run.fail("records stay readable", error))?
                .state;
            let fires: Vec<_> = record
                .events
                .iter()
                .filter(|event| event.action == "Expire")
                .collect();
            if let Some(out_of_state) = fires.iter().find(|fire| fire.from_status != "Waiting") {
                return Err(run.fail(
                    "a timer fires only while its record is in its state",
                    format!("{name}: Expire fired in {}", out_of_state.from_status),
                ));
            }
            let was_moved = job.tenant.as_str() == "gamma" && moved.contains(&job.id);
            let wanted_fires = if was_moved { 0..=1 } else { 1..=1 };
            if record.status != "Done" || !wanted_fires.contains(&fires.len()) {
                return Err(run.fail(
                    "every record leaves its timed state on its timer, once",
                    format!(
                        "{name} is {} after {} timer fires{}",
                        record.status,
                        fires.len(),
                        if was_moved { " (moved)" } else { "" }
                    ),
                ));
            }
            let wanted_by = if job.tenant.as_str() == "gamma" {
                STATE_TIMER_SERVICE.to_string()
            } else {
                starter(job)
            };
            let by = fired_as
                .remove(&(job.tenant.to_string(), job.id.clone()))
                .unwrap_or_default();
            if by.len() != fires.len()
                || by
                    .iter()
                    .any(|by| by.as_deref() != Some(wanted_by.as_str()))
            {
                return Err(run.fail(
                    "a timer fires as whoever armed it",
                    format!("{name}: fired as {by:?}, wanted {wanted_by}; moves seen: {seen:?}"),
                ));
            }
            for fire in fires {
                fired_at
                    .entry(fire.timestamp.to_rfc3339())
                    .or_default()
                    .push(name.clone());
            }
        }
        if let Some((at, fired)) = fired_at
            .iter()
            .find(|(_, fired)| fired.len() > FIRES_AT_ONCE)
        {
            return Err(run.fail(
                "due timers fire a few at a time",
                format!("{} timers fired at {at}: {fired:?}", fired.len()),
            ));
        }
        let waiting = second
            .booted
            .get(&(TenantId::new("gamma"), "Job".to_string()))
            .map_or(0, |counts| counts.waiting);
        if waiting != 0 {
            return Err(run.fail(
                "the boot arms the records in a timed state that have no row",
                format!("gamma/Job: the boot left {waiting} records waiting"),
            ));
        }
        Ok(())
    })?;
    second.kill(&world, Duration::ZERO);
    Ok(())
}
