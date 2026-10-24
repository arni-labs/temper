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
//!
//! A timer fires as someone: the caller of the dispatch that reconciles, else
//! whoever armed the timer it replaces, else (a record nobody armed a timer
//! for, as when an install adds a timeout to its type) the kernel's
//! [`STATE_TIMER_SERVICE`]. A timeout action that binds its caller's identity
//! (`authenticated_subject`) refuses a service caller by design, so such a
//! record waits for its next action, whose caller arms it.
//!
//! The walks that reconcile every record of a type, after an install and at
//! boot, are in `state_timer_follow`.

use std::sync::Arc;

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::state_timeouts::{Reservation, declared_timer, rearm_clock_start, remaining_budget};
use crate::entity_actor::{EntityState, RehydrateHook};
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// The service a timer fires as when nobody armed it: a record found in a
/// timed state by an install or a boot with no timer armed and no row.
const STATE_TIMER_SERVICE: &str = "state-timer";

/// What a reconcile left a record with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimerFollow {
    /// The timer its state declares, or none.
    Settled,
    /// The record could not be read; the walk must retry it.
    RetryRead,
    /// Its state declares a timer whose action binds its caller's identity,
    /// and nobody armed it: the record's next action arms it as that caller.
    WaitsForCaller,
}

impl ServerState {
    /// Make `entity_id`'s armed timer the one `current`'s state declares, or
    /// none. A timer already armed for that declaration is kept; any other is
    /// cancelled and its row removed. A timer armed here fires as `armed_by`,
    /// else as whoever armed the timer it replaces, else as the
    /// [`STATE_TIMER_SERVICE`] when its action does not bind a caller.
    pub(crate) async fn reconcile_state_timers(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        current: &EntityState,
        armed_by: Option<&AgentContext>,
    ) -> TimerFollow {
        let declared = {
            let Ok(registry) = self.registry.read() else {
                return TimerFollow::Settled;
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
            return TimerFollow::Settled;
        };
        let known = armed_by.cloned().or_else(|| {
            self.armed_state_timer(tenant, entity_type, entity_id)
                .map(|armed| armed.agent_ctx)
        });
        let identity = match known {
            Some(identity) => identity,
            None if self.action_binds_caller(tenant, entity_type, &st.on_timeout) => {
                return TimerFollow::WaitsForCaller;
            }
            None => AgentContext::for_service(STATE_TIMER_SERVICE),
        };
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
        TimerFollow::Settled
    }

    /// Whether `action` binds a parameter from its caller's identity, which a
    /// service caller cannot give (`resolve_authenticated_params`).
    fn action_binds_caller(&self, tenant: &TenantId, entity_type: &str, action: &str) -> bool {
        self.transition_table_for_dispatch(tenant, entity_type)
            .is_ok_and(|table| {
                table
                    .action_contracts
                    .get(action)
                    .is_some_and(|contract| !contract.param_sources.is_empty())
            })
    }

    /// Whether `entity_type`'s spec declares any state timeout.
    pub(super) fn declares_state_timeouts(&self, tenant: &TenantId, entity_type: &str) -> bool {
        self.registry.read().ok().is_some_and(|registry| {
            registry
                .get_spec(tenant, entity_type)
                .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
        })
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
        if !self.declares_state_timeouts(tenant, entity_type)
            && !self.has_armed_state_timer(tenant, entity_type, entity_id)
        {
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
}
