//! The invariants of the state-timer simulation, checked against the timer
//! rows and the records' states.

use super::{Job, Run, World, settle, status, timer_action};
use temper_server::ServerState;

/// Invariant: after `jobs` rehydrate, each has exactly the timer row its
/// current state declares.
pub(super) async fn check_rows_follow_state(
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

/// Invariant: `jobs`, known to be in `state` and not read since the boot, have
/// its timer rows (none when deleted). Checked without reading them, since a
/// read rehydrates a record and arms its timer, so only the boot armed these.
pub(super) fn check_rows_before_reads(
    world: &World,
    jobs: &[Job],
    state: &str,
    run: &Run,
) -> Result<(), String> {
    for job in jobs {
        let row = world.rows.row(job.tenant.as_str(), &job.id);
        let (current, wanted) = if run.is_deleted(job) {
            ("<deleted>", None)
        } else {
            (state, timer_action(state).map(|_| state))
        };
        if row.as_ref().map(|row| row.state.as_str()) != wanted {
            return Err(run.fail(
                "the boot arms every record's timer",
                format!(
                    "before any read: {}/{} is {current} but its timer row is {:?}",
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
pub(super) async fn check_all_done(
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
