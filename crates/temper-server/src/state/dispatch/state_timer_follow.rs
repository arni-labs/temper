//! Walks that make every record of a timed type follow its state's timer.
//!
//! An install that changes a type's spec, or gives a type its first timeout,
//! can leave records in a timed state with the timer of another state or
//! none. Its walk reconciles every record of the type
//! ([`ServerState::reconcile_state_timers`]), so every record in a timed state
//! ends with its timer armed, or waits for a caller (see
//! [`TimerFollow::WaitsForCaller`]).
//!
//! At boot, rows and records without rows follow their replayed state through
//! one shared fire gate. The walk reports new arms, overdue timers and records
//! that still need a caller, per tenant and type.
//!
//! A record in memory is reconciled with its actor's state. One that is not is
//! replayed from the store without starting an actor, so a walk over many
//! records does not leave them all resident.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

use temper_runtime::tenant::TenantId;

use super::state_timer_reconcile::{Follow, TimerFollow};
use crate::entity_actor::{EntityState, recover_entity_state_for_walk};
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::projection_backfill::transition_table_for;

/// The wait before retrying a type after the store failed to list or read
/// its records. It doubles up to [`FOLLOW_RETRY_MAX`], so a store that stays
/// down is asked about once a minute, not in a tight loop.
const FOLLOW_RETRY_FIRST: Duration = Duration::from_secs(1);
const FOLLOW_RETRY_MAX: Duration = Duration::from_secs(60);

/// Who armed the timer of a record whose row the boot dropped (its state no
/// longer declares a timeout), so the record's new timer fires as them.
pub(super) type BootCallers = BTreeMap<(TenantId, String, String), AgentContext>;

/// What the boot did with one timed type's timers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BootTimerCounts {
    /// Rows armed again.
    pub rearmed: usize,
    /// Rows removed: their state no longer declares a timeout.
    pub dropped: usize,
    /// Records whose timeout action still needs a caller.
    pub waiting: usize,
    /// Missing timers armed from the recorded state clock.
    pub armed: usize,
    /// Timers already due when armed or rearmed.
    pub overdue: usize,
    /// Scheduled actions (`schedule`, `schedule_at`) armed again from their
    /// rows.
    pub scheduled: usize,
}

/// The boot's [`BootTimerCounts`] by tenant and entity type.
pub type BootTimers = BTreeMap<(TenantId, String), BootTimerCounts>;

/// While held, one tenant's changed types wait out of every walk (see
/// [`ServerState::hold_timer_follow`]). Dropping it, on success or failure,
/// makes them ready for the next walk.
pub struct TimerFollowHold {
    server: ServerState,
    tenant: TenantId,
}

impl Drop for TimerFollowHold {
    fn drop(&mut self) {
        if let Ok(mut registry) = self.server.registry.write() {
            registry.finish_install(&self.tenant);
        }
    }
}

impl ServerState {
    /// Make the timers of every record of each type whose spec an install
    /// changed follow the state the record is in now, including a type that
    /// had no timeout before. Failed listings and reads remain pending until
    /// the store recovers. Startup installs defer to `start_runtime`'s
    /// boot walk, preserving saved callers and its shared fire gate.
    pub async fn follow_spec_changes(&self) {
        let changed = match self.registry.write() {
            Ok(mut registry) => {
                // Only tenants whose saved identities are restored may follow
                // installs. Keep other tenants queued without blocking them.
                let complete = self
                    .timer_recovery_complete
                    .load(std::sync::atomic::Ordering::Acquire);
                let recovered = self
                    .timer_recovered_tenants
                    .read()
                    .expect("timer recovery lock poisoned");
                registry.take_changed_types_for(|tenant| complete || recovered.contains(tenant))
            }
            Err(_) => return,
        };
        self.follow_types(changed, &BootCallers::new(), None).await;
    }

    /// Hold `tenant`'s changed types back from every walk, any tenant's,
    /// until the returned hold is dropped: while an installation into it
    /// registers specs ahead of the policies and modules its timers need.
    pub fn hold_timer_follow(&self, tenant: &TenantId) -> TimerFollowHold {
        self.registry
            .write()
            .expect("registry lock poisoned")
            .begin_install(tenant);
        TimerFollowHold {
            server: self.clone(),
            tenant: tenant.clone(),
        }
    }

    /// [`Self::follow_spec_changes`] in the background, for an install that
    /// answers before every record is read.
    pub fn spawn_follow_spec_changes(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let server = self.clone();
        runtime.spawn(async move { server.follow_spec_changes().await }); // determinism-ok: install follow-up runs in the background
    }

    /// Reconcile every record of each of `types` from their recorded clocks, and
    /// return how many records of each type wait for their next action. A type
    /// whose records the store cannot list or read is tried again after a
    /// wait. Repeating a successful reconcile is idempotent; an unreadable
    /// record may have no timer at all, so the walk must retain responsibility
    /// for retrying it.
    pub(super) async fn follow_types(
        &self,
        mut types: Vec<(TenantId, String)>,
        callers: &BootCallers,
        gate: Option<Arc<Semaphore>>,
    ) -> BootTimers {
        let mut wait = FOLLOW_RETRY_FIRST;
        let mut waiting_by_type = BootTimers::new();
        loop {
            let mut pending = Vec::new();
            for (tenant, entity_type) in types {
                if let Err(error) = self.ensure_type_indexed(&tenant, &entity_type).await {
                    tracing::warn!(tenant = %tenant, entity_type, %error,
                        retry_in_secs = wait.as_secs(),
                        "records of a timed type could not be listed; trying again");
                    pending.push((tenant, entity_type));
                    continue;
                }
                let ids = self.list_entity_ids(&tenant, &entity_type);
                let mut counted = BootTimerCounts::default();
                let mut unreadable = 0usize;
                for entity_id in &ids {
                    let key = (tenant.clone(), entity_type.clone(), entity_id.clone());
                    let follow = Follow {
                        armed_by: callers.get(&key),
                        gate: gate.clone(),
                        rows_recovered: gate.is_some(),
                    };
                    let followed = self
                        .reconcile_stored_record(&tenant, &entity_type, entity_id, follow)
                        .await;
                    counted.waiting += usize::from(followed == TimerFollow::WaitsForCaller);
                    if let TimerFollow::Armed { overdue } = followed {
                        counted.armed += 1;
                        counted.overdue += usize::from(overdue);
                    }
                    unreadable += usize::from(followed == TimerFollow::RetryRead);
                }
                let total = waiting_by_type
                    .entry((tenant.clone(), entity_type.clone()))
                    .or_default();
                total.armed += counted.armed;
                total.overdue += counted.overdue;
                total.waiting = counted.waiting;
                if unreadable > 0 {
                    tracing::warn!(tenant = %tenant, entity_type, unreadable,
                        retry_in_secs = wait.as_secs(),
                        "records of a timed type could not be read; trying again");
                    pending.push((tenant, entity_type));
                    continue;
                }
                tracing::info!(tenant = %tenant, entity_type, records = ids.len(),
                    waiting_for_caller = counted.waiting, armed = counted.armed, overdue = counted.overdue,
                    "state timers follow the records' states");
                if counted.waiting > 0 {
                    tracing::warn!(tenant = %tenant, entity_type, waiting = counted.waiting,
                        "records in a timed state wait for their next action: the timeout action binds its caller and nobody armed it");
                }
            }
            if pending.is_empty() {
                return waiting_by_type;
            }
            tokio::time::sleep(wait).await; // determinism-ok: retry wait on the runtime clock
            wait = (wait * 2).min(FOLLOW_RETRY_MAX);
            types = pending;
        }
    }

    /// Reconcile one record: with its actor's state when it is in memory,
    /// else with its state replayed from the store. A record that cannot be
    /// read remains pending, even when it has no timer to check it again.
    pub(super) async fn reconcile_stored_record(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        follow: Follow<'_>,
    ) -> TimerFollow {
        let Some(current) = self
            .current_record_state(tenant, entity_type, entity_id)
            .await
        else {
            return TimerFollow::RetryRead;
        };
        if current.total_event_count == 0 {
            return TimerFollow::Settled;
        }
        self.follow_state_timer(tenant, entity_type, entity_id, &current, follow)
            .await
    }

    /// The record's state now, without starting its actor when it has none.
    async fn current_record_state(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<EntityState> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        let resident = self
            .actor_registry
            .read()
            .is_ok_and(|actors| actors.get(&key).is_some_and(|actor| !actor.is_closed()));
        let journal = self.event_journal();
        let (Some((store, backend)), false) = (journal, resident) else {
            return match self
                .get_tenant_entity_state(tenant, entity_type, entity_id)
                .await
            {
                Ok(response) => Some(response.state),
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "record not readable; its timer is not reconciled");
                    None
                }
            };
        };
        let table = transition_table_for(self, tenant, entity_type)?;
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        match recover_entity_state_for_walk(
            tenant.as_str(),
            entity_type,
            entity_id,
            &table,
            &store,
            backend,
            &serde_json::json!({}),
            blob_store.as_ref(),
        )
        .await
        {
            Ok(state) => Some(state),
            Err(error) => {
                tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                    "record not replayable; its timer is not reconciled");
                None
            }
        }
    }
}

/// Report the completed recovery across all tenants.
pub(super) fn log_boot_timers(counts: &BootTimers) {
    let by_type = counts
            .iter()
            .map(|((tenant, entity_type), counts)| {
                format!(
                    "{tenant}/{entity_type} rearmed={} dropped={} waiting={} scheduled={} armed={} overdue={}",
                    counts.rearmed, counts.dropped, counts.waiting, counts.scheduled, counts.armed, counts.overdue
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
    // The boot check reads this summary through the WARN+ OTLP log export.
    tracing::warn!(
        rearmed = counts.values().map(|c| c.rearmed).sum::<usize>(),
        dropped = counts.values().map(|c| c.dropped).sum::<usize>(),
        waiting = counts.values().map(|c| c.waiting).sum::<usize>(),
        scheduled = counts.values().map(|c| c.scheduled).sum::<usize>(),
        armed = counts.values().map(|c| c.armed).sum::<usize>(),
        overdue = counts.values().map(|c| c.overdue).sum::<usize>(),
        by_type,
        "state timers after boot"
    );
}
