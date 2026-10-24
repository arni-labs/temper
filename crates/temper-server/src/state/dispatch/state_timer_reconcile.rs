//! A record's armed timer follows its current state.
//!
//! [`ServerState::reconcile_state_timers`] is the one place that decides which
//! timer a record has armed: the one its current state declares, or none. It is
//! called whenever the record's state may have moved without a transition
//! arming or cancelling its timer:
//!
//! - after the record rehydrates (its actor starts from stored events), since
//!   replay under a newer spec can land it in another state than the one its
//!   timer was armed for;
//! - after a spec install, for every record of each type whose spec changed;
//! - when a timer fires on a record that is no longer in its state;
//! - after a dispatch that neither entered nor reset a timed state, after a
//!   create, and after a delete.
//!
//! It is idempotent: a record whose armed timer is already the one its state
//! declares keeps it, so any number of calls leave one timer armed. The timer's
//! clock starts when the record entered its state, so a timer re-armed after a
//! restart or a rehydration waits only the rest of its budget, or fires at once.

use std::sync::Arc;

use temper_runtime::persistence::StateTimerRecord;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use tokio::sync::Semaphore;

use super::state_timeouts::{Reservation, declared_timer, rearm_clock_start, remaining_budget};
use super::state_timer_rows::row_armed_by;
use crate::entity_actor::{EntityState, RehydrateHook};
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// The service a timer fires as when nothing else armed it: a record found in
/// a timed state by a rehydration or a spec install, with no timer armed and
/// no dispatch to take an identity from.
const STATE_TIMER_SERVICE: &str = "state-timer";

impl ServerState {
    /// Make `entity_id`'s armed timer the one `current`'s state declares, or
    /// none. A timer already armed for that declaration is kept; any other is
    /// cancelled and its row removed. A timer armed here fires as `armed_by`,
    /// else as whoever armed the timer it replaces, else as the
    /// [`STATE_TIMER_SERVICE`].
    pub(crate) async fn reconcile_state_timers(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        current: &EntityState,
        armed_by: Option<&AgentContext>,
    ) {
        let declared = {
            let Ok(registry) = self.registry.read() else {
                return;
            };
            registry.get_spec(tenant, entity_type).and_then(|spec| {
                spec.automaton
                    .state_timeouts
                    .iter()
                    .find(|timeout| timeout.state == current.status)
                    .cloned()
            })
        };
        let now = sim_now();
        // A timer that keeps the record in its state without resetting the
        // clock fires once: once it ran, the state wants no timer.
        let wanted = declared
            .and_then(|st| rearm_clock_start(&st, current, now).map(|started| (st, started)));
        let Some((st, started_at)) = wanted else {
            self.cancel_state_timer(tenant, entity_type, entity_id, current.sequence_nr)
                .await;
            return;
        };
        let identity = armed_by
            .cloned()
            .or_else(|| {
                self.armed_state_timer(tenant, entity_type, entity_id)
                    .map(|armed| armed.agent_ctx)
            })
            .unwrap_or_else(|| AgentContext::for_service(STATE_TIMER_SERVICE));
        let timer = declared_timer(
            tenant,
            entity_type,
            entity_id,
            &st,
            current,
            &identity,
            started_at,
        );
        let delay = remaining_budget(started_at, st.after_seconds, now);
        if self
            .start_armed_timer(timer, delay, Reservation::Missing)
            .await
        {
            crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                tenant.as_str(),
                entity_type,
                &st.state,
                if delay.is_zero() {
                    "overdue"
                } else {
                    "budgeted"
                },
            );
        }
    }

    /// Read `entity_id` and make its armed timer follow its state. A deleted
    /// record is not brought back to read it, and one that cannot be read
    /// keeps its timer: both are checked again when the timer fires.
    pub(crate) async fn reconcile_record_state_timers(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        armed_by: Option<&AgentContext>,
    ) {
        if !self
            .ensure_entity_loaded(tenant, entity_type, entity_id)
            .await
        {
            return;
        }
        match self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            Ok(current) => {
                self.reconcile_state_timers(
                    tenant,
                    entity_type,
                    entity_id,
                    &current.state,
                    armed_by,
                )
                .await;
            }
            Err(error) => tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                "record not readable; its timer is not reconciled"),
        }
    }

    /// The hook an entity actor runs when it starts from stored events: it
    /// makes the record's timer follow the state it rehydrated into. None for
    /// a type that declares no timeout and a record with no timer armed.
    pub(crate) fn state_timer_rehydrate_hook(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<RehydrateHook> {
        let declares_timeouts = self.registry.read().ok().is_some_and(|registry| {
            registry
                .get_spec(tenant, entity_type)
                .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
        });
        if !declares_timeouts && !self.has_armed_state_timer(tenant, entity_type, entity_id) {
            return None;
        }
        let server = self.clone();
        let (tenant, entity_type, entity_id) = (
            tenant.clone(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        Some(Arc::new(move |rehydrated: &EntityState| {
            let (server, rehydrated) = (server.clone(), rehydrated.clone());
            let (tenant, entity_type, entity_id) =
                (tenant.clone(), entity_type.clone(), entity_id.clone());
            // determinism-ok: background reconcile, as timers are spawned; under
            // simulation it runs on the single-threaded test runtime.
            tokio::spawn(async move {
                server
                    .reconcile_state_timers(&tenant, &entity_type, &entity_id, &rehydrated, None)
                    .await;
            });
        }))
    }

    /// Read the record of a row whose state no longer declares a timeout, and
    /// arm what its current state declares, as whoever armed the row. Waits
    /// its turn on the boot `gate`.
    pub(super) fn spawn_boot_reconcile(
        &self,
        tenant: &TenantId,
        row: &StateTimerRecord,
        gate: Arc<Semaphore>,
    ) {
        let armed_by = row_armed_by(row);
        let (server, tenant) = (self.clone(), tenant.clone());
        let (entity_type, entity_id) = (row.entity_type.clone(), row.entity_id.clone());
        // determinism-ok: boot follow-up, as boot timers are spawned.
        tokio::spawn(async move {
            let _permit = gate.acquire_owned().await.ok();
            server
                .reconcile_record_state_timers(&tenant, &entity_type, &entity_id, armed_by.as_ref())
                .await;
        });
    }

    /// Make the timers of every record of each type whose spec an install
    /// changed follow the state the record is in now. Records not in memory
    /// are read, so each lands in the state replay under the new spec gives
    /// it. Each changed type is followed once.
    pub async fn follow_spec_changes(&self) {
        let changed = match self.registry.write() {
            Ok(mut registry) => registry.take_changed_types(),
            Err(_) => return,
        };
        for (tenant, entity_type) in changed {
            let ids = self.list_entity_ids_lazy(&tenant, &entity_type).await;
            tracing::info!(tenant = %tenant, entity_type, records = ids.len(),
                "spec changed; reconciling its records' state timers");
            for entity_id in ids {
                self.reconcile_record_state_timers(&tenant, &entity_type, &entity_id, None)
                    .await;
            }
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
}
