//! A record's armed timer follows its current state.
//!
//! [`ServerState::reconcile_state_timers`] is the one place that decides which
//! timer a record has armed: the one its current state declares, or none. It is
//! called whenever the record's state may have moved without a transition
//! arming or cancelling its timer:
//!
//! - after the record rehydrates (its actor starts from stored events), since
//!   a newer spec can declare another timer, or none, for the state it
//!   replays into than the one its timer was armed for;
//! - after a spec install, for every record of each type whose spec changed;
//! - when a timer fires on a record that is no longer in its state;
//! - after a dispatch that neither entered nor reset a timed state, after a
//!   create, and after a delete.
//!
//! Every reconcile arms a record in a timed state that has no timer, the
//! boot walk's and a rehydration's included. A record can be in a timed state
//! with no timer row: it entered the state before rows were kept, its arming
//! was lost (a row write or a projection that failed, a dropped request), or
//! an install walk the last process did not finish. Its timer is armed from
//! when it entered the state, read from its journal, so it waits only the
//! rest of its budget, or is due at once. At boot those fire through the boot
//! walk's gate, a few at a time.
//!
//! It is idempotent: a record whose armed timer is already the one its state
//! declares keeps it, so any number of calls leave one timer armed. The timer's
//! clock starts when the record entered its state, so a timer re-armed after a
//! restart or a rehydration waits only the rest of its budget, or fires at once.
//!
//! A timer fires as someone: the caller of the dispatch that reconciles, else
//! whoever armed the timer it replaces (its row's `armed_by`), else (a record
//! nobody armed a timer for, as when an install adds a timeout to its type,
//! or one with no row at boot: the journal does not keep who made each move)
//! the kernel's [`STATE_TIMER_SERVICE`]. A timeout action that binds its
//! caller's identity (`authenticated_subject`) refuses a service caller by
//! design, so such a record waits for its next action, whose caller arms it.
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
/// timed state by an install with no timer armed and no row.
const STATE_TIMER_SERVICE: &str = "state-timer";

/// What a reconcile left a record with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TimerFollow {
    /// The timer its state declares, or none, and it had that one already.
    Settled,
    /// The record could not be read; the walk must retry it.
    RetryRead,
    /// Its state declares a timer it did not have, and that timer is armed
    /// now, from when the record entered the state. `overdue`: its budget had
    /// run out, so it fires at once (at boot, through the boot gate).
    Armed {
        state: String,
        on_timeout: String,
        overdue: bool,
    },
    /// Its state declares a timer whose action binds its caller's identity,
    /// and nobody armed it: the record's next action arms it as that caller.
    WaitsForCaller { state: String, on_timeout: String },
}

/// How a reconcile treats the record: who its new timer fires as, and the
/// boot gate it fires through.
#[derive(Clone, Default)]
pub(crate) struct Follow<'a> {
    /// The caller of the dispatch that reconciles, or whoever armed the row
    /// the boot dropped.
    pub(crate) armed_by: Option<&'a AgentContext>,
    /// The boot walk's fire gate.
    pub(crate) gate: Option<Arc<tokio::sync::Semaphore>>,
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
        let follow = Follow {
            armed_by,
            ..Follow::default()
        };
        self.follow_state_timer(tenant, entity_type, entity_id, current, follow)
            .await
    }

    /// [`Self::reconcile_state_timers`] as `follow` says.
    pub(super) async fn follow_state_timer(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        current: &EntityState,
        follow: Follow<'_>,
    ) -> TimerFollow {
        let Follow { armed_by, gate } = follow;
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
        let armed = self.armed_state_timer(tenant, entity_type, entity_id);
        let known = armed_by
            .cloned()
            .or_else(|| armed.as_ref().map(|armed| armed.agent_ctx.clone()));
        let identity = match known {
            Some(identity) => identity,
            None if self.action_binds_caller(tenant, entity_type, &st.on_timeout) => {
                return TimerFollow::WaitsForCaller {
                    state: st.state,
                    on_timeout: st.on_timeout,
                };
            }
            None => AgentContext::for_service(STATE_TIMER_SERVICE),
        };
        let mut timer = declared_timer(
            tenant,
            entity_type,
            entity_id,
            &st,
            current,
            &identity,
            started_at,
        );
        timer.boot_gate = gate.or_else(|| armed.and_then(|armed| armed.boot_gate));
        let delay = remaining_budget(started_at, st.after_seconds, now);
        if !self
            .start_armed_timer(timer, delay, Reservation::Missing)
            .await
        {
            return TimerFollow::Settled;
        }
        let overdue = delay.is_zero();
        crate::runtime_metrics::record_state_timeout_armed_on_hydration(
            tenant.as_str(),
            entity_type,
            &st.state,
            if overdue { "overdue" } else { "budgeted" },
        );
        TimerFollow::Armed {
            state: st.state,
            on_timeout: st.on_timeout,
            overdue,
        }
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
    /// makes the record's timer follow the state it rehydrated into, arming
    /// the one that state declares when the record has none. None for a type
    /// that declares no timeout and a record with no timer armed.
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
            let follow = Follow::default();
            // determinism-ok: background reconcile, as timers are spawned; under
            // simulation it runs on the single-threaded test runtime.
            tokio::spawn(async move {
                server
                    .follow_state_timer(&tenant, &entity_type, &entity_id, &rehydrated, follow)
                    .await;
            });
        }))
    }
}
