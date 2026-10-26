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
//! - when a timer fires on a record that is no longer in its state, or whose
//!   journal moved past the sequence the timer was armed at (another writer
//!   fired it, or an action it does not reset on came first);
//! - after a dispatch that neither entered nor reset a timed state, after a
//!   create, and after a delete.
//!
//! Only a fresh event arms a record that has no timer, and so no row: a
//! dispatch, a create, or an install that changed the record's spec. A
//! rehydration and a boot follow only a timer the record already has, from
//! its row ([`Arming::FromRow`]); a record in a timed state with no row waits
//! for its next action, and the boot walk counts it.
//!
//! It is idempotent: a record whose armed timer is already the one its state
//! declares keeps it, so any number of calls leave one timer armed. The timer's
//! clock starts when the record entered its state, so a timer re-armed after a
//! restart or a rehydration waits only the rest of its budget, or fires at once.
//!
//! A timer fires as someone: the caller of the dispatch that reconciles, else
//! whoever armed the timer it replaces (its row's `armed_by`), else (a record
//! nobody armed a timer for, as when an install adds a timeout to its type)
//! the kernel's
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
/// timed state by an install with no timer armed and no row.
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
    /// Its state declares a timer, but it has no timer and no row, and a
    /// rehydration or a boot does not arm one: its next action does.
    WaitsForRow,
}

/// Whether a reconcile may arm a timer for a record that has none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Arming {
    /// A fresh event says the record is in its state now: a dispatch, a
    /// create, a delete, a timer that found its record elsewhere, or an
    /// install that changed the record's spec. A missing timer is armed.
    #[default]
    Any,
    /// A rehydration or a boot: only a timer the record has (armed from its
    /// row, or the row the boot dropped) follows its state. A record with
    /// neither waits ([`TimerFollow::WaitsForRow`]), so a boot never arms
    /// records that had no timer, and never fires them all at once.
    FromRow,
}

/// How a reconcile treats the record: who its new timer fires as, the boot
/// gate it fires through, and whether it may arm a record that has no timer.
#[derive(Clone, Default)]
pub(crate) struct Follow<'a> {
    /// The caller of the dispatch that reconciles, or whoever armed the row
    /// the boot dropped.
    pub(crate) armed_by: Option<&'a AgentContext>,
    /// The boot walk's fire gate.
    pub(crate) gate: Option<Arc<tokio::sync::Semaphore>>,
    pub(crate) arming: Arming,
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
        let Follow {
            armed_by,
            gate,
            arming,
        } = follow;
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
        // Rows that follow commits are the timers the journal armed: one the
        // record's events left for this state is the timer it has, even when
        // the process never armed it (the move's answer was lost, or the
        // actor rebuilt from its journal). Read even with a live arm: a
        // committed reset can have superseded it without post-dispatch work.
        let row = self
            .committed_state_timer_row(tenant, entity_type, entity_id)
            .await
            .filter(|row| row.state == st.state && row.sequence_nr <= current.sequence_nr);
        let committed = row.as_ref().and_then(|row| {
            match super::state_timer_rows::armed_timer(
                tenant,
                row,
                &st.on_timeout,
                st.after_seconds,
            ) {
                Ok(timer) => Some(timer),
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "committed state timer row not decodable");
                    None
                }
            }
        });
        if arming == Arming::FromRow && armed.is_none() && armed_by.is_none() && row.is_none() {
            return TimerFollow::WaitsForRow;
        }
        let known = committed
            .as_ref()
            .map(|timer| timer.agent_ctx.clone())
            .or_else(|| armed_by.cloned())
            .or_else(|| armed.as_ref().map(|armed| armed.agent_ctx.clone()))
            .or_else(|| row.as_ref().and_then(super::state_timer_rows::row_armed_by));
        let identity = match known {
            Some(identity) => identity,
            None if self.action_binds_caller(tenant, entity_type, &st.on_timeout) => {
                return TimerFollow::WaitsForCaller;
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
        let reservation = if let Some(committed) = committed {
            timer = committed;
            // Keep the durable arm's clock, payload, caller and row identity,
            // but fence its replacement at the record we just reconciled.
            // Non-resetting actions advance the journal without a new arm.
            timer.sequence_nr = current.sequence_nr;
            Reservation::Committed
        } else {
            Reservation::Missing
        };
        timer.boot_gate = gate.or_else(|| armed.and_then(|armed| armed.boot_gate));
        let delay = remaining_budget(timer.started_at, st.after_seconds, now);
        if self.start_armed_timer(timer, delay, reservation).await {
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
    /// makes the record's timer, if it has one, follow the state it
    /// rehydrated into ([`Arming::FromRow`]). None for a type that declares no
    /// timeout and a record with no timer armed.
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
                let follow = Follow {
                    arming: Arming::FromRow,
                    ..Follow::default()
                };
                server
                    .follow_state_timer(&tenant, &entity_type, &entity_id, &rehydrated, follow)
                    .await;
            });
        }))
    }
}
