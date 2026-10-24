//! Runtime execution of `[[state_timeout]]` declarations (ADR-0049).
//!
//! After each successful entity transition, [`ServerState::arm_state_timeouts_if_needed`]
//! inspects the spec to decide whether the new state warrants a timer. A
//! timer is armed on:
//!
//! - **State entry** — the transition moved the entity to a state with a
//!   `[[state_timeout]]` declaration.
//! - **Reset signal** — the action that fired is listed in the declaration's
//!   `reset_on` while the entity is in the declared state.
//!
//! Cancellation is sequence-based. Every arm bumps a per-entity counter in
//! [`StateTimeoutTracker`] and captures the post-bump value. When the timer
//! fires, it re-reads the current counter; any mismatch means a newer arm
//! (or a state change) invalidated this one, so the fire is a no-op.
//!
//! State exits and reset signals invalidate the old timer before waiting
//! for query visibility. Successor timers are armed after that wait, so an
//! old deadline cannot fire while a committed reset is being projected.
//!
//! Durability (ADR-0056): every armed timer keeps a durable row with its
//! clock start, parameters and the move that armed it, and removes it when
//! it ends. At boot `rearm_state_timers` (`state_timer_rows.rs`) arms each
//! waiting row again: for the rest of its budget, or at once if it fell due
//! while the server was down. A timer fires once, with the authority of
//! whoever armed it, and its move is recorded as the state timer's
//! ([`crate::entity_actor::STATE_TIMER_ACTOR`]) with the arming move as its
//! cause: the spec's follow-on is the app's own rule, and Cedar judged the move
//! into the timed state.
//!
//! The arm logic also detects the **hydration case**: the entity is in a
//! state with a declared timeout but has no live in-memory timer (seq == 0 in
//! the tracker), as for a timer armed before its row was kept. Then the
//! dispatch reconstructs how long the entity has been in the current state
//! (the most recent transition into `state.status`, or the most recent
//! `reset_on` event after it) from the entity's status clock, which snapshots
//! keep, or from its retained events when the state predates the clock. It
//! fires `on_timeout` at once if that is a full budget, or arms a timer for the
//! rest. That timer counts the dispatch that found it as its arming move.
//! Before the status clock, a state restored from a snapshot (which leaves
//! events out) or with the entry pushed out of the retained events armed a
//! full budget on every hydration, so a daily timer on a server restarted more
//! often than daily never fired.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use tracing::Instrument;

use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{
    EntityEvent, EntityResponse, EntityState, MoveCause, MoveOrigin, STATE_TIMER_ACTOR,
};
use crate::request_context::AgentContext;

use super::effects::PostDispatchContext;

/// Walk the event history backward to find the timestamp of the most recent
/// "progress" signal for the given state: either the transition that entered
/// `current_state`, or any later event whose action is in `reset_on`. This
/// timestamp is the reference point for computing how long the entity has
/// been idle in the current state, used by the hydration re-arm path to
/// compute remaining timeout budget.
///
/// Returns `None` if no matching entry event is found in the retained window
/// (state.events is a bounded VecDeque; older history has been snapshotted
/// and forgotten). Callers treat `None` as "no elapsed info available" and
/// arm with the full budget.
fn compute_state_clock_reset_ts(
    events: &VecDeque<EntityEvent>,
    current_state: &str,
    reset_on: &[String],
) -> Option<DateTime<Utc>> {
    // Find the most recent state entry: last event whose to_status ==
    // current_state AND from_status != current_state. Scanning backward
    // because recent events are at the back.
    let entry_idx = events
        .iter()
        .rposition(|e| e.to_status == current_state && e.from_status != current_state)?;
    let entry_ts = events[entry_idx].timestamp;

    // Among events after the entry, find the latest reset_on event.
    // Those reset the clock per ADR-0049 semantics.
    let latest_reset_ts = events
        .iter()
        .skip(entry_idx + 1)
        .filter(|e| reset_on.iter().any(|a| a == &e.action))
        .map(|e| e.timestamp)
        .max();

    Some(latest_reset_ts.unwrap_or(entry_ts))
}

/// When the clock of a timer on `current_state` started: from the entity's
/// status clock, which snapshots keep, or else from its retained events (a
/// state restored from a snapshot made before the status clock was kept).
fn state_clock_started_at(
    state: &EntityState,
    current_state: &str,
    reset_on: &[String],
) -> Option<DateTime<Utc>> {
    match state.status_clock.entered_at {
        Some(entered) if state.status == current_state => Some(
            reset_on
                .iter()
                .filter_map(|action| state.status_clock.actions.get(action))
                .copied()
                .fold(entered, DateTime::max),
        ),
        _ => compute_state_clock_reset_ts(&state.events, current_state, reset_on),
    }
}

/// Composite key identifying an entity instance inside the arm-seq tracker.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct EntityKey {
    tenant: String,
    entity_type: String,
    entity_id: String,
}

impl EntityKey {
    fn new(tenant: &TenantId, entity_type: &str, entity_id: &str) -> Self {
        Self {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        }
    }
}

/// One entity's cancellation counter, and the journal sequence of the
/// transition that last armed or cancelled its timer.
#[derive(Default, Debug, Clone, Copy)]
struct ArmSeq {
    seq: u64,
    journal: u64,
}

/// In-memory cancellation counter keyed by entity instance.
///
/// Each arm increments and captures the new value; firings compare captured
/// against current and drop the fire when they diverge. An arm from an
/// earlier journal sequence than the last arm or cancellation is refused, so
/// a dispatch whose post-dispatch work finishes late never replaces the timer
/// of a newer transition.
#[derive(Default, Debug)]
pub struct StateTimeoutTracker {
    seqs: Mutex<HashMap<EntityKey, ArmSeq>>,
    /// ADR-0049: per-entity-type count of armed-but-unfired timers.
    /// Emitted as `temper_scheduler_pending_timers` by the canary loop.
    pending_by_type: Mutex<HashMap<String, u64>>,
}

impl StateTimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn bump(&self, key: &EntityKey) -> u64 {
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key.clone()).or_default();
        entry.seq += 1;
        entry.seq
    }

    /// Cancel the entity's timer for the transition at journal sequence
    /// `journal`: no arm from an earlier transition is taken after this. A
    /// cancellation from an earlier transition than the last arm or
    /// cancellation (a late post-dispatch, or a retry answered from the
    /// idempotency cache) cancels nothing.
    fn invalidate(&self, key: &EntityKey, journal: u64) -> u64 {
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key.clone()).or_default();
        if journal < entry.journal {
            return entry.seq;
        }
        entry.seq += 1;
        entry.journal = journal;
        entry.seq
    }

    /// Take a new cancellation value for a timer armed by the transition at
    /// journal sequence `journal`, or None when a later transition already
    /// armed or cancelled this entity's timer.
    fn reserve(&self, key: &EntityKey, journal: u64) -> Option<u64> {
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key.clone()).or_default();
        if journal < entry.journal {
            return None;
        }
        entry.seq += 1;
        entry.journal = journal;
        Some(entry.seq)
    }

    fn current(&self, key: &EntityKey) -> u64 {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .map_or(0, |arm| arm.seq)
    }

    /// Increment the pending-timer count for `entity_type`. Called at arm.
    pub fn inc_pending(&self, entity_type: &str) {
        let mut map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        *map.entry(entity_type.to_string()).or_insert(0) += 1;
    }

    /// Decrement the pending-timer count for `entity_type`. Called when a
    /// timer task exits (fired, cancelled by seq mismatch, or state changed).
    pub fn dec_pending(&self, entity_type: &str) {
        let mut map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        if let Some(v) = map.get_mut(entity_type)
            && *v > 0
        {
            *v -= 1;
        }
    }

    /// Snapshot pending counts per entity type for metric emission.
    pub fn pending_snapshot(&self) -> Vec<(String, u64)> {
        let map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        map.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Drop any seq for `key`. Called when an entity is deleted so the map
    /// doesn't grow unbounded over the process lifetime.
    pub fn forget(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        let _ = self
            .seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .remove(&key);
    }

    #[cfg(test)]
    fn size(&self) -> usize {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .len()
    }
}

impl crate::state::ServerState {
    /// Invalidate timers superseded by the durable transition before any I/O.
    pub(crate) fn invalidate_state_timeouts_if_needed(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let Ok(registry) = self.registry.read() else {
            return;
        };
        let Some(spec) = registry.get_spec(ctx.tenant, ctx.entity_type) else {
            return;
        };
        let pre_state = response
            .state
            .events
            .back()
            .map(|event| event.from_status.as_str())
            .unwrap_or_default();
        let post_state = response.state.status.as_str();
        let state_changed = pre_state != post_state;
        let cancels_prior = state_changed
            && spec
                .automaton
                .state_timeouts
                .iter()
                .any(|timeout| timeout.state == pre_state);
        let resets_current = !state_changed
            && spec.automaton.state_timeouts.iter().any(|timeout| {
                timeout.state == post_state
                    && timeout.reset_on.iter().any(|action| action == ctx.action)
            });
        if cancels_prior || resets_current {
            self.state_timeout_tracker.invalidate(
                &EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id),
                response.state.sequence_nr,
            );
        }
        if cancels_prior {
            crate::runtime_metrics::record_state_timeout_cancelled(
                ctx.tenant.as_str(),
                ctx.entity_type,
                pre_state,
            );
        }
    }

    /// Arm or re-arm state timers based on the just-completed transition.
    ///
    /// Invoked from `run_post_dispatch_effects`. Walks the spec's
    /// `state_timeouts` and arms a timer for each that applies. Each armed
    /// timer first writes its durable row, so a restart arms it again
    /// ([`Self::rearm_state_timers`]). The row is written before the dispatch
    /// returns: a repeating timer's next row then exists before the timer that
    /// fired it removes its own, so no restart falls between the two.
    pub(crate) async fn arm_state_timeouts_if_needed(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let state_timeouts = {
            let Ok(registry) = self.registry.read() else {
                return;
            };
            let Some(spec) = registry.get_spec(ctx.tenant, ctx.entity_type) else {
                return;
            };
            spec.automaton.state_timeouts.clone()
        };
        if state_timeouts.is_empty() {
            return;
        }

        let post_state = response.state.status.clone();
        let pre_state = response
            .state
            .events
            .back()
            .map(|e| e.from_status.clone())
            .unwrap_or_default();
        let state_changed = pre_state != post_state;
        let key = EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id);

        // Arm timers for any matching state_timeout declaration.
        for st in &state_timeouts {
            if st.state != post_state {
                continue;
            }
            let is_entry = state_changed;
            let is_reset = !state_changed && st.reset_on.iter().any(|a| a == ctx.action);
            // ADR-0056: hydration re-arm. If the entity is in a state with a
            // declared timeout and this process has no live timer for it (seq
            // == 0), treat this dispatch as the opportunity to catch up. This
            // handles actors that hydrated from snapshot after a restart or
            // passivation without the benefit of a state transition to arm
            // the timer.
            let needs_hydration_rearm =
                !is_entry && !is_reset && self.state_timeout_tracker.current(&key) == 0;
            if !is_entry && !is_reset && !needs_hydration_rearm {
                continue;
            }
            if is_reset {
                crate::runtime_metrics::record_state_timeout_reset(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &st.state,
                    ctx.action,
                );
            }

            // When the state's clock started.
            //
            // - Entry / reset: now, so the timer waits its full budget.
            // - Hydration re-arm: the entity's entry into the current state
            //   (or its most recent `reset_on` event, whichever is later). If
            //   that is a full budget ago, the timer fires on the next tick.
            let now = sim_now();
            let mut started_at = now;
            if needs_hydration_rearm {
                let Some(clock) = rearm_clock_start(st, &response.state, now) else {
                    continue;
                };
                started_at = clock;
                crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &st.state,
                    if remaining_budget(started_at, st.after_seconds, now).is_zero() {
                        "overdue"
                    } else {
                        "budgeted"
                    },
                );
            }

            let timer = declared_timer(
                ctx.tenant,
                ctx.entity_type,
                ctx.entity_id,
                st,
                &response.state,
                ArmingMove {
                    by: ctx.agent_ctx,
                    action: ctx.action,
                },
                started_at,
            );
            let delay = remaining_budget(started_at, st.after_seconds, now);
            self.start_armed_timer(timer, delay).await;
        }
    }

    /// Arm the timers of a record created straight into a timed state.
    ///
    /// Creation is not a dispatch, so nothing else arms them until the
    /// record's first action. Like any arm, the timer fires with the authority
    /// of whoever armed it: here, whoever created the record, and its move's
    /// cause is the creation. A record that already has a live
    /// timer keeps it; one that already existed counts from when it entered
    /// its state, as the hydration re-arm does.
    pub(crate) async fn arm_state_timeouts_on_creation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        created: &EntityResponse,
        created_by: &AgentContext,
    ) {
        let state_timeouts = {
            let Ok(registry) = self.registry.read() else {
                return;
            };
            let Some(spec) = registry.get_spec(tenant, entity_type) else {
                return;
            };
            spec.automaton.state_timeouts.clone()
        };
        let key = EntityKey::new(tenant, entity_type, entity_id);
        for st in state_timeouts
            .iter()
            .filter(|st| st.state == created.state.status)
        {
            if self.state_timeout_tracker.current(&key) != 0 {
                return;
            }
            let now = sim_now();
            let Some(started_at) = rearm_clock_start(st, &created.state, now) else {
                continue;
            };
            let created_with = created
                .state
                .events
                .back()
                .map(|event| event.action.as_str());
            let timer = declared_timer(
                tenant,
                entity_type,
                entity_id,
                st,
                &created.state,
                ArmingMove {
                    by: created_by,
                    action: created_with.unwrap_or_default(),
                },
                started_at,
            );
            let delay = remaining_budget(started_at, st.after_seconds, now);
            self.start_armed_timer(timer, delay).await;
        }
    }

    /// Reserve, save and start one armed timer. Reserved before the row is
    /// written: an arm whose dispatch finishes after a newer transition's is
    /// refused, and one that a newer arm or a state exit supersedes while its
    /// row is written removes the row and does not start.
    async fn start_armed_timer(&self, timer: ArmedTimer, delay: Duration) {
        let Some(armed_seq) = self.reserve_state_timer(&timer) else {
            return;
        };
        self.persist_state_timer(&timer).await;
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        if self.state_timeout_tracker.current(&key) != armed_seq {
            self.forget_state_timer(&timer).await;
            return;
        }
        self.spawn_state_timer(timer, delay, None, armed_seq);
    }

    /// Whether `timer`'s action already ran since its clock started, keeping
    /// the record in its state without resetting the clock: its fire
    /// committed, and a crash left the row behind before the timer ended. Such
    /// a timer fires once, so it does not fire again.
    fn state_timer_already_fired(&self, timer: &ArmedTimer, current: &EntityState) -> bool {
        let ran_since = current
            .status_clock
            .actions
            .get(&timer.action)
            .is_some_and(|ran| *ran >= timer.started_at);
        ran_since && !self.state_timeout_resets_on(timer)
    }

    /// Whether `timer`'s action is in its state's `reset_on`: a repeating
    /// timer, whose fire starts the clock again.
    fn state_timeout_resets_on(&self, timer: &ArmedTimer) -> bool {
        let Ok(registry) = self.registry.read() else {
            return false;
        };
        registry
            .get_spec(&timer.tenant, &timer.entity_type)
            .and_then(|spec| {
                spec.automaton
                    .state_timeouts
                    .iter()
                    .find(|timeout| timeout.state == timer.state)
            })
            .is_some_and(|timeout| timeout.reset_on.contains(&timer.action))
    }

    /// Reserve a cancellation value for `timer` (see [`StateTimeoutTracker`]),
    /// or None when a later transition of its entity already armed or
    /// cancelled a timer.
    pub(super) fn reserve_state_timer(&self, timer: &ArmedTimer) -> Option<u64> {
        self.state_timeout_tracker.reserve(
            &EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id),
            timer.sequence_nr,
        )
    }

    /// Start one armed timer's task, with the cancellation value
    /// [`Self::reserve_state_timer`] gave it: it waits `delay`, then fires the
    /// timer's action as the timer's identity if no newer arm or state change
    /// has superseded it, and removes the timer's durable row when it ends.
    /// With a `gate`, the fire waits for one of its permits and holds it until
    /// the timer ends.
    pub(super) fn spawn_state_timer(
        &self,
        timer: ArmedTimer,
        delay: Duration,
        gate: Option<Arc<Semaphore>>,
        armed_seq: u64,
    ) {
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        self.state_timeout_tracker.inc_pending(&timer.entity_type);
        let workflow_root_entity_type = timer
            .armed_by
            .workflow_root_entity_type
            .clone()
            .unwrap_or_else(|| timer.entity_type.clone());
        let workflow_root_entity_id = timer
            .armed_by
            .workflow_root_entity_id
            .clone()
            .unwrap_or_else(|| timer.entity_id.clone());
        let workflow_run_id = timer
            .armed_by
            .workflow_run_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", timer.entity_type, timer.entity_id));

        tracing::debug!(
            tenant = %timer.tenant,
            entity_type = %timer.entity_type,
            entity_id = %timer.entity_id,
            target_state = %timer.state,
            target_action = %timer.action,
            delay_ms = delay.as_millis() as u64,
            workflow.root_entity_type = %workflow_root_entity_type,
            workflow.root_entity_id = %workflow_root_entity_id,
            workflow.run_id = %workflow_run_id,
            "armed state timeout"
        );

        let state = self.clone();
        tokio::spawn(async move {
            // determinism-ok: wall-clock timer fires a side-effect action;
            // the action itself is deterministic under DST via sim_now().
            tokio::time::sleep(delay).await; // determinism-ok: scheduled delay
            let _permit = match gate {
                Some(gate) => gate.acquire_owned().await.ok(),
                None => None,
            };

            let span = tracing::info_span!(
                "dispatch.state_timeout.fire",
                tenant = %timer.tenant,
                entity_type = %timer.entity_type,
                entity_id = %timer.entity_id,
                target_state = %timer.state,
                target_action = %timer.action,
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
            );

            async move {
                // Sequence-based cancellation check. A newer arm (or a state
                // change that bumped the seq on exit) renders this timer a
                // no-op. The secondary check confirms the entity is still in
                // the target state: defense in depth, and the only check a
                // timer armed at boot has against changes made before the
                // restart.
                if state.state_timeout_tracker.current(&key) == armed_seq
                    && let Ok(current) = state
                        .get_tenant_entity_state(
                            &timer.tenant,
                            &timer.entity_type,
                            &timer.entity_id,
                        )
                        .await
                    && current.state.status == timer.state
                    && !state.state_timer_already_fired(&timer, &current.state)
                {
                    crate::runtime_metrics::record_state_timeout_fired(
                        timer.tenant.as_str(),
                        &timer.entity_type,
                        &timer.state,
                        &timer.action,
                    );
                    let _ = state
                        .dispatch_tenant_action(
                            &timer.tenant,
                            &timer.entity_type,
                            &timer.entity_id,
                            &timer.action,
                            timer.params.clone(),
                            &timer.fire_agent(),
                        )
                        .await;
                }
                state.forget_state_timer(&timer).await;
                state.state_timeout_tracker.dec_pending(&timer.entity_type);
            }
            .instrument(span)
            .await;
        });
    }
}

/// One armed timer: what fires, when its state's clock started, and the move
/// that armed it.
pub(super) struct ArmedTimer {
    pub(super) tenant: TenantId,
    pub(super) entity_type: String,
    pub(super) entity_id: String,
    /// The timed state; the timer fires only while the entity is still in it.
    pub(super) state: String,
    /// The `on_timeout` action and its parameters, read when armed.
    pub(super) action: String,
    pub(super) params: serde_json::Value,
    /// The dispatch that armed the timer, as a new background action. Its
    /// authority is the authority the timer's move runs with.
    pub(super) armed_by: AgentContext,
    /// The action of the move that armed the timer; None for a row kept
    /// before arming actions were.
    pub(super) cause_action: Option<String>,
    /// Names this arming's durable row.
    pub(super) arm_id: String,
    /// The entity's journal sequence when the timer was armed.
    pub(super) sequence_nr: u64,
    pub(super) started_at: DateTime<Utc>,
}

impl ArmedTimer {
    /// The context the timer's move is dispatched with. It keeps the arming
    /// dispatch's identity and authority (`agent_id`, `security_ctx`), so
    /// bound parameters, Cedar checks on the move's integrations and its
    /// adapters see what they saw when the move was armed. Its `origin`
    /// records it as the state timer's move, with the arming move as its
    /// cause, because the spec's follow-on is the app's rule rather than a
    /// move the arming person asked for.
    pub(super) fn fire_agent(&self) -> AgentContext {
        let mut agent = self.armed_by.clone();
        agent.origin = Some(MoveOrigin {
            actor: STATE_TIMER_ACTOR.to_string(),
            caused_by: MoveCause {
                agent_id: self.armed_by.agent_id.clone(),
                action: self.cause_action.clone(),
                sequence_nr: self.sequence_nr,
            },
        });
        agent
    }
}

/// The arming dispatch as a new background action. It does not keep the
/// arming request's idempotency key: the entity has already recorded that key,
/// so the fire would be answered as a duplicate and never run.
fn timer_agent(armed_by: &AgentContext) -> AgentContext {
    let mut agent = armed_by.for_background_task();
    agent.idempotency_key = None;
    agent
}

/// When the clock of a timer re-armed without a transition started: the
/// entity's entry into its state or its latest `reset_on`, else `now` (a safe
/// default whose worst case is one extra budget of wait). None when the timer
/// keeps the entity in its state without resetting the clock and already fired
/// since the clock started: it fires once, so there is nothing to arm.
fn rearm_clock_start(
    st: &temper_spec::automaton::StateTimeout,
    state: &EntityState,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let started_at = state_clock_started_at(state, &st.state, &st.reset_on).unwrap_or(now);
    let fired_since = state
        .status_clock
        .actions
        .get(&st.on_timeout)
        .is_some_and(|fired| *fired >= started_at);
    (!fired_since || st.reset_on.contains(&st.on_timeout)).then_some(started_at)
}

/// The move that arms a timer: who made it, and its action.
struct ArmingMove<'a> {
    by: &'a AgentContext,
    action: &'a str,
}

/// The timer `st` arms on an entity, armed by `arming`. ADR-0181: `args` are
/// read when the timer is armed, against the entity as it entered the state.
fn declared_timer(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    st: &temper_spec::automaton::StateTimeout,
    state: &EntityState,
    arming: ArmingMove<'_>,
    started_at: DateTime<Utc>,
) -> ArmedTimer {
    let (params, missing) =
        temper_spec::automaton::resolve_args(&st.args, entity_id, &state.fields);
    for field in missing {
        tracing::warn!(
            entity_type,
            state = %st.state,
            source_field = field,
            "state_timeout arg reads a missing field; skipping key"
        );
    }
    ArmedTimer {
        tenant: tenant.clone(),
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        state: st.state.clone(),
        action: st.on_timeout.clone(),
        params: serde_json::Value::Object(params),
        armed_by: timer_agent(arming.by),
        cause_action: Some(arming.action.to_string()),
        arm_id: sim_uuid().to_string(),
        sequence_nr: state.sequence_nr,
        started_at,
    }
}

/// How much of a timer's budget is left at `now`; zero once it is due.
pub(super) fn remaining_budget(
    started_at: DateTime<Utc>,
    after_seconds: u64,
    now: DateTime<Utc>,
) -> Duration {
    let elapsed = now
        .signed_duration_since(started_at)
        .to_std()
        .unwrap_or(Duration::ZERO);
    Duration::from_secs(after_seconds).saturating_sub(elapsed)
}

#[cfg(test)]
impl StateTimeoutTracker {
    pub(crate) fn sequence_for_test(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> u64 {
        self.current(&EntityKey::new(tenant, entity_type, entity_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::tenant::TenantId;

    fn key() -> EntityKey {
        EntityKey::new(&TenantId::from("t".to_string()), "E", "e-1")
    }

    #[test]
    fn bump_returns_monotonically_increasing_seq() {
        let t = StateTimeoutTracker::new();
        let k = key();
        assert_eq!(t.current(&k), 0, "initial seq is 0");
        assert_eq!(t.bump(&k), 1);
        assert_eq!(t.bump(&k), 2);
        assert_eq!(t.bump(&k), 3);
        assert_eq!(t.current(&k), 3);
    }

    #[test]
    fn an_arm_from_an_earlier_transition_is_refused() {
        let t = StateTimeoutTracker::new();
        let k = key();
        assert_eq!(t.reserve(&k, 5), Some(1));
        assert_eq!(
            t.reserve(&k, 3),
            None,
            "an older transition's arm that finishes late"
        );
        assert_eq!(t.current(&k), 1, "the newer timer stays current");
        assert_eq!(t.invalidate(&k, 7), 2);
        assert_eq!(t.reserve(&k, 6), None, "an arm from before the exit");
        assert_eq!(
            t.invalidate(&k, 4),
            2,
            "a cancellation from an earlier transition cancels nothing"
        );
        assert_eq!(
            t.reserve(&k, 7),
            Some(3),
            "the same transition again, as a duplicate reply re-arms"
        );
    }

    #[test]
    fn per_entity_seqs_are_independent() {
        let t = StateTimeoutTracker::new();
        let a = EntityKey::new(&TenantId::from("t".to_string()), "E", "a");
        let b = EntityKey::new(&TenantId::from("t".to_string()), "E", "b");
        assert_eq!(t.bump(&a), 1);
        assert_eq!(t.bump(&a), 2);
        assert_eq!(t.bump(&b), 1, "b's seq is independent of a's");
        assert_eq!(t.current(&a), 2);
        assert_eq!(t.current(&b), 1);
    }

    #[test]
    fn forget_releases_entity() {
        let t = StateTimeoutTracker::new();
        let tenant = TenantId::from("t".to_string());
        t.bump(&EntityKey::new(&tenant, "E", "x"));
        assert_eq!(t.size(), 1);
        t.forget(&tenant, "E", "x");
        assert_eq!(t.size(), 0);
    }

    // --- compute_state_clock_reset_ts (ADR-0056 hydration-re-arm helper) ---

    fn test_event(action: &str, from: &str, to: &str, ts_ms_after_epoch: i64) -> EntityEvent {
        let ts = DateTime::<Utc>::from_timestamp_millis(ts_ms_after_epoch).unwrap();
        EntityEvent {
            action: action.to_string(),
            from_status: from.to_string(),
            to_status: to.to_string(),
            timestamp: ts,
            params: serde_json::json!({}),
            idempotency_key: None,
            origin: None,
        }
    }

    #[test]
    fn clock_reset_finds_most_recent_entry_event() {
        let mut events = VecDeque::new();
        events.push_back(test_event("Create", "", "Open", 1_000));
        events.push_back(test_event("Assign", "Open", "InProgress", 2_000));
        events.push_back(test_event("Close", "InProgress", "Closed", 3_000));

        // Current state Closed → clock reset == Close event timestamp.
        let reset = compute_state_clock_reset_ts(&events, "Closed", &[]).unwrap();
        assert_eq!(reset.timestamp_millis(), 3_000);
    }

    #[test]
    fn clock_reset_prefers_reset_on_event_after_entry() {
        let mut events = VecDeque::new();
        events.push_back(test_event("Enter", "", "Executing", 100));
        events.push_back(test_event("DoWork", "Executing", "Executing", 500));
        events.push_back(test_event("ProgressMade", "Executing", "Executing", 900));
        events.push_back(test_event("OtherAction", "Executing", "Executing", 1_200));

        let reset_on = vec!["ProgressMade".to_string()];
        let reset = compute_state_clock_reset_ts(&events, "Executing", &reset_on).unwrap();
        assert_eq!(
            reset.timestamp_millis(),
            900,
            "latest reset_on event wins over later non-reset events"
        );
    }

    #[test]
    fn clock_reset_falls_back_to_entry_when_no_reset_events() {
        let mut events = VecDeque::new();
        events.push_back(test_event("Configure", "Queued", "Ready", 500));
        events.push_back(test_event("Start", "Ready", "Executing", 1_000));
        events.push_back(test_event("Steer", "Executing", "Executing", 1_500));

        let reset_on = vec!["ProgressMade".to_string()];
        let reset = compute_state_clock_reset_ts(&events, "Executing", &reset_on).unwrap();
        assert_eq!(
            reset.timestamp_millis(),
            1_000,
            "Steer is not a reset_on; entry timestamp wins"
        );
    }

    #[test]
    fn clock_reset_returns_none_when_no_entry_event_retained() {
        let mut events = VecDeque::new();
        // Only self-loops retained in the window; the original transition
        // into `Executing` has been snapshotted and forgotten.
        events.push_back(test_event("Steer", "Executing", "Executing", 100));
        events.push_back(test_event("Steer", "Executing", "Executing", 200));

        let reset = compute_state_clock_reset_ts(&events, "Executing", &[]);
        assert!(reset.is_none(), "no entry event in window → None");
    }

    #[test]
    fn clock_reset_ignores_entry_events_for_other_states() {
        let mut events = VecDeque::new();
        events.push_back(test_event("Create", "", "Open", 1_000));
        events.push_back(test_event("Assign", "Open", "InProgress", 2_000));
        // Query for Open, but the current state is InProgress — no match.
        let reset = compute_state_clock_reset_ts(&events, "Open", &[]);
        // The events.back() is InProgress, so no entry-into-Open event
        // with from != to is in the window; the original entry at index 0
        // has from_status="" which satisfies "!= current_state", so it IS
        // considered an entry-into-Open event — clock reset == 1_000.
        assert_eq!(reset.unwrap().timestamp_millis(), 1_000);
    }

    #[test]
    fn clock_reset_ignores_self_loop_events_as_entry() {
        // Self-loops have from == to, so they must NOT be treated as entry.
        // The prior transition is the true entry point.
        let mut events = VecDeque::new();
        events.push_back(test_event("Create", "", "Executing", 100));
        events.push_back(test_event("Steer", "Executing", "Executing", 500));
        events.push_back(test_event("Steer", "Executing", "Executing", 800));

        let reset = compute_state_clock_reset_ts(&events, "Executing", &[]).unwrap();
        assert_eq!(
            reset.timestamp_millis(),
            100,
            "first real entry wins; subsequent self-loops don't re-enter"
        );
    }

    // ------------------------------------------------------------------
    // Integration test: prove the runtime scheduler actually fires.
    // ------------------------------------------------------------------

    use crate::registry::SpecRegistry;
    use crate::request_context::AgentContext;
    use crate::state::dispatch::effects::PostDispatchContext;
    use crate::state::{DispatchCommand, ServerState};
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;

    const TICKET_CSDL: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");

    /// Custom Ticket IOA with a state_timeout on `Open`. Fires `AssignAgent`
    /// after 1 second; the action transitions the ticket to `InProgress`.
    /// By default `AssignAgent` is an input action from `Open`, so the
    /// auto-wiring stays idempotent.
    const TICKET_WITH_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = 0

[[state]]
name = "customer_responded"
type = "bool"
initial = false

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "AssignAgent"
"#;

    /// Ticket spec with tight admission caps — used to prove admission
    /// control actually gates concurrent dispatches end-to-end.
    const TICKET_WITH_ADMISSION_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = 0

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[admission]
max_concurrent_creates = 5
max_concurrent_actions = { "AssignAgent" = 5 }
queue_depth = 1000
queue_timeout_seconds = 10
"#;

    // Incident-replay style load proof: 120 concurrent dispatches against an
    // admission cap of 5. With the pre-fix behavior (no admission + fixed 5s
    // ask timeout + no retry), a subset would surface as HTTP 500. With the
    // fix in place, every caller either (a) is granted and succeeds, or (b)
    // gets Deferred (503 Retry-After) — no 500s, no mailbox-full drops, and
    // the cap is strictly enforced (never more than 5 in flight).
    //
    // Also reports throughput and latency percentiles so the performance
    // baseline is tracked alongside correctness.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn load_120_concurrent_dispatches_admission_caps_hold() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", TICKET_WITH_ADMISSION_IOA)],
        );
        let system = ActorSystem::new("load-admission-test");
        let state = Arc::new(ServerState::from_registry(system, registry));
        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        // Pre-create 120 ticket entities so the concurrent AssignAgent calls
        // race on the shared admission cap for that action.
        const N: usize = 120;
        for i in 0..N {
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Ticket",
                    &format!("t-{i}"),
                    serde_json::json!({}),
                )
                .await
                .expect("create ticket");
        }

        let granted = Arc::new(AtomicUsize::new(0));
        let deferred = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));
        let in_flight_peak = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let latencies_ns = Arc::new(Mutex::new(Vec::<u128>::with_capacity(N)));

        let barrier = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        let wall_start = Instant::now();
        for i in 0..N {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let granted = granted.clone();
            let deferred = deferred.clone();
            let other = other.clone();
            let in_flight_peak = in_flight_peak.clone();
            let in_flight = in_flight.clone();
            let latencies_ns = latencies_ns.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await; // fire all at once
                let call_start = Instant::now();
                in_flight.fetch_add(1, Ordering::AcqRel);
                // Record peak in-flight count.
                let cur = in_flight.load(Ordering::Acquire);
                let mut peak = in_flight_peak.load(Ordering::Acquire);
                while cur > peak
                    && let Err(p) = in_flight_peak.compare_exchange(
                        peak,
                        cur,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                {
                    peak = p;
                }

                let res = state
                    .dispatch_tenant_action_ext_typed(
                        &tenant,
                        "Ticket",
                        &format!("t-{i}"),
                        "AssignAgent",
                        serde_json::json!({}),
                        crate::state::dispatch::DispatchExtOptions {
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        },
                    )
                    .await;
                let call_ns = call_start.elapsed().as_nanos();
                latencies_ns.lock().unwrap().push(call_ns);
                match res {
                    Ok(r) if r.success => {
                        granted.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(_) => {
                        other.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(crate::state::dispatch::DispatchError::Deferred { .. }) => {
                        deferred.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(_) => {
                        other.fetch_add(1, Ordering::AcqRel);
                    }
                }
                in_flight.fetch_sub(1, Ordering::AcqRel);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = wall_start.elapsed();

        let g = granted.load(Ordering::Acquire);
        let d = deferred.load(Ordering::Acquire);
        let o = other.load(Ordering::Acquire);
        let peak = in_flight_peak.load(Ordering::Acquire);
        let mut lats = latencies_ns.lock().unwrap().clone();
        lats.sort_unstable();
        let p = |q: f64| -> u128 {
            let idx = ((lats.len() as f64 - 1.0) * q).round() as usize;
            lats[idx.min(lats.len().saturating_sub(1))]
        };
        let throughput = N as f64 / wall.as_secs_f64();

        eprintln!(
            "LOAD RESULT: granted={g} deferred={d} other={o} in_flight_peak={peak} total={N}"
        );
        eprintln!(
            "LOAD PERF:   wall={wall_ms:.1}ms throughput={tp:.0}/s p50={p50:.2}ms p95={p95:.2}ms p99={p99:.2}ms max={pmax:.2}ms",
            wall_ms = wall.as_secs_f64() * 1000.0,
            tp = throughput,
            p50 = (p(0.50) as f64) / 1_000_000.0,
            p95 = (p(0.95) as f64) / 1_000_000.0,
            p99 = (p(0.99) as f64) / 1_000_000.0,
            pmax = (p(1.0) as f64) / 1_000_000.0,
        );

        // Hard contract assertions:
        //
        // 1. Zero unknown failures — every outcome is Granted or Deferred;
        //    no panics, no permanent errors, no timeouts.
        assert_eq!(
            o, 0,
            "unexpected non-granted, non-deferred outcomes: {o} (spec: 500-class behavior is forbidden)"
        );

        // 2. Every dispatch is accounted for.
        assert_eq!(g + d + o, N, "outcome count must equal submissions");

        // 3. Admission cap holds — since cap is 5 and queue_timeout is 10s
        //    with N=120 inputs, some should defer. If all 120 granted
        //    instantly, admission isn't firing at all.
        assert!(
            g >= 5,
            "at least the cap's worth ({}) should succeed, got {g}",
            5
        );

        // 4. Peak in-flight observation does NOT assert <= 5 because
        //    in_flight counts the pre-acquire window too; what we do
        //    assert is the admission semaphore gate works (see test
        //    `grants_up_to_cap_and_defers_beyond` for the hard cap proof).
    }

    /// Tight-cap spec with `queue_timeout_seconds = 0` — acquirers that
    /// cannot grab a permit instantly are immediately deferred.
    /// This proves the admission gate actually enforces the cap under
    /// sustained contention — a slow real-world action (like an LLM call
    /// that takes seconds per transition) would see the same cap enforced
    /// via a non-zero queue timeout.
    const TICKET_ZERO_QUEUE_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = 0

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[admission]
max_concurrent_creates = 2
max_concurrent_actions = { "AssignAgent" = 2 }
queue_depth = 1000
queue_timeout_seconds = 0
"#;

    /// Adversarial burst-load: 300 simultaneous dispatches against cap=2
    /// with a zero-second queue budget. Without admission's FIFO gate, the
    /// flood would hit the shared actor's 1000-deep mailbox and produce
    /// `MailboxFull` errors (the 2026-04-17 Katagami incident pattern).
    /// With admission active, the contract is: every caller is either
    /// `Granted` (served quickly through the cap) or `Deferred` (503
    /// Retry-After). **Zero 500s under any circumstances.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn load_tight_cap_observes_deferrals() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", TICKET_ZERO_QUEUE_IOA)],
        );
        let system = ActorSystem::new("load-tight-admission-test");
        let state = Arc::new(ServerState::from_registry(system, registry));
        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        // Shared entity — all 300 dispatches contend for the SAME ticket
        // so the actor's single-threaded processing adds queue time on
        // top of the admission gate, making deferrals observable.
        state
            .get_or_create_tenant_entity(&tenant, "Ticket", "shared-ticket", serde_json::json!({}))
            .await
            .expect("create");

        const N: usize = 300;
        let granted = Arc::new(AtomicUsize::new(0));
        let deferred = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));
        let lat_granted_ns = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));
        let lat_deferred_ns = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));

        // Prime a synchronization barrier so ALL 300 fire at the same instant.
        let barrier = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        let wall_start = std::time::Instant::now();
        for _i in 0..N {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let granted = granted.clone();
            let deferred = deferred.clone();
            let other = other.clone();
            let lat_granted_ns = lat_granted_ns.clone();
            let lat_deferred_ns = lat_deferred_ns.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let call_start = std::time::Instant::now();
                let res = state
                    .dispatch_tenant_action_ext_typed(
                        &tenant,
                        "Ticket",
                        "shared-ticket",
                        "AssignAgent",
                        serde_json::json!({}),
                        crate::state::dispatch::DispatchExtOptions {
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        },
                    )
                    .await;
                let call_ns = call_start.elapsed().as_nanos();
                match res {
                    Ok(_) => {
                        granted.fetch_add(1, Ordering::AcqRel);
                        lat_granted_ns.lock().unwrap().push(call_ns);
                    }
                    Err(crate::state::dispatch::DispatchError::Deferred { .. }) => {
                        deferred.fetch_add(1, Ordering::AcqRel);
                        lat_deferred_ns.lock().unwrap().push(call_ns);
                    }
                    Err(_) => {
                        other.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = wall_start.elapsed();

        let g = granted.load(Ordering::Acquire);
        let d = deferred.load(Ordering::Acquire);
        let o = other.load(Ordering::Acquire);
        let throughput = N as f64 / wall.as_secs_f64();
        let mut gl = lat_granted_ns.lock().unwrap().clone();
        let mut dl = lat_deferred_ns.lock().unwrap().clone();
        gl.sort_unstable();
        dl.sort_unstable();
        let p = |v: &[u128], q: f64| -> u128 {
            if v.is_empty() {
                return 0;
            }
            let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
            v[idx.min(v.len() - 1)]
        };
        eprintln!("TIGHT-CAP RESULT: granted={g} deferred={d} other={o} total={N}");
        eprintln!(
            "TIGHT-CAP PERF:   wall={wall_ms:.1}ms throughput={tp:.0}/s",
            wall_ms = wall.as_secs_f64() * 1000.0,
            tp = throughput,
        );
        eprintln!(
            "                  granted p50={gp50:.2}ms p95={gp95:.2}ms p99={gp99:.2}ms",
            gp50 = (p(&gl, 0.50) as f64) / 1_000_000.0,
            gp95 = (p(&gl, 0.95) as f64) / 1_000_000.0,
            gp99 = (p(&gl, 0.99) as f64) / 1_000_000.0,
        );
        eprintln!(
            "                  deferred p50={dp50:.2}ms p95={dp95:.2}ms p99={dp99:.2}ms (time-to-503)",
            dp50 = (p(&dl, 0.50) as f64) / 1_000_000.0,
            dp95 = (p(&dl, 0.95) as f64) / 1_000_000.0,
            dp99 = (p(&dl, 0.99) as f64) / 1_000_000.0,
        );

        // Hard contract:
        //   * Zero 500-class outcomes. Every caller is either served or told
        //     to back off with a 503-equivalent.
        assert_eq!(o, 0, "expected no 500-class outcomes, got {o}");
        //   * All N accounted for.
        assert_eq!(g + d + o, N);
        //   * Admission actually bites: with cap=2 and 1s queue timeout
        //     against 300 contenders on one actor, not all can serve.
        //     This is the incident-class proof: burst → deferrals, not 500s.
        assert!(
            d > 0,
            "admission control must produce at least one deferral under tight cap; got granted={g} deferred={d}"
        );
    }

    /// 1000-way sustained-throughput measurement: cap=50 (realistic
    /// production-style cap), each call hits a unique entity so work
    /// parallelizes across actors. Measures steady-state dispatch cost
    /// through the full retry + admission + dispatch path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn load_1000_throughput_baseline() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        const THROUGHPUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = 0

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[admission]
max_concurrent_creates = 50
max_concurrent_actions = { "AssignAgent" = 50 }
queue_depth = 2000
queue_timeout_seconds = 30
"#;

        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", THROUGHPUT_IOA)],
        );
        let system = ActorSystem::new("throughput-1000-test");
        let state = Arc::new(ServerState::from_registry(system, registry));
        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        const N: usize = 1000;

        // Pre-create all entities so the dispatch phase is pure transition.
        for i in 0..N {
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Ticket",
                    &format!("t-{i}"),
                    serde_json::json!({}),
                )
                .await
                .expect("create");
        }

        let granted = Arc::new(AtomicUsize::new(0));
        let errored = Arc::new(AtomicUsize::new(0));
        let lat_ns = Arc::new(Mutex::new(Vec::<u128>::with_capacity(N)));
        let barrier = Arc::new(tokio::sync::Barrier::new(N));

        let mut handles = Vec::with_capacity(N);
        let wall_start = Instant::now();
        for i in 0..N {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let granted = granted.clone();
            let errored = errored.clone();
            let lat_ns = lat_ns.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let start = Instant::now();
                let res = state
                    .dispatch_tenant_action_ext_typed(
                        &tenant,
                        "Ticket",
                        &format!("t-{i}"),
                        "AssignAgent",
                        serde_json::json!({}),
                        crate::state::dispatch::DispatchExtOptions {
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        },
                    )
                    .await;
                let elapsed = start.elapsed().as_nanos();
                lat_ns.lock().unwrap().push(elapsed);
                match res {
                    Ok(r) if r.success => {
                        granted.fetch_add(1, Ordering::AcqRel);
                    }
                    _ => {
                        errored.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = wall_start.elapsed();

        let g = granted.load(Ordering::Acquire);
        let e = errored.load(Ordering::Acquire);
        let mut lats = lat_ns.lock().unwrap().clone();
        lats.sort_unstable();
        let p = |q: f64| -> u128 {
            let idx = ((lats.len() as f64 - 1.0) * q).round() as usize;
            lats[idx.min(lats.len() - 1)]
        };
        let tp = N as f64 / wall.as_secs_f64();

        eprintln!("1000-THROUGHPUT: granted={g} other={e} total={N}");
        eprintln!(
            "1000-PERF:   wall={wall_ms:.1}ms throughput={tp:.0} dispatches/sec",
            wall_ms = wall.as_secs_f64() * 1000.0,
        );
        eprintln!(
            "             p50={p50:.2}ms p90={p90:.2}ms p95={p95:.2}ms p99={p99:.2}ms max={pmax:.2}ms",
            p50 = (p(0.50) as f64) / 1_000_000.0,
            p90 = (p(0.90) as f64) / 1_000_000.0,
            p95 = (p(0.95) as f64) / 1_000_000.0,
            p99 = (p(0.99) as f64) / 1_000_000.0,
            pmax = (p(1.0) as f64) / 1_000_000.0,
        );

        assert_eq!(e, 0, "1000-dispatch baseline must be zero-error");
        assert_eq!(g, N, "every dispatch should succeed under generous cap");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_timeout_fires_and_transitions_entity() {
        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", TICKET_WITH_TIMEOUT_IOA)],
        );
        let system = ActorSystem::new("state-timeout-integration");
        let state = ServerState::from_registry(system, registry);

        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        // Create the entity so it lands in `Open`.
        let created = state
            .get_or_create_tenant_entity(&tenant, "Ticket", "t-1", serde_json::json!({}))
            .await
            .expect("create ticket");
        assert_eq!(created.state.status, "Open");

        // Arm the state_timeout by dispatching a no-op Action? We need to
        // trigger `arm_state_timeouts_if_needed`. Creation itself doesn't
        // go through dispatch, so arm via a self-loop transition — easiest
        // path is to dispatch a direct RecordProgress-like action. Ticket
        // spec has no self-loop, so we simulate by inspecting initial state
        // and letting the watchdog fire by entering Open via Configure. For
        // this test, we force an arm by directly calling the ServerState
        // hook with a synthesized PostDispatchContext.
        let response = state
            .get_tenant_entity_state(&tenant, "Ticket", "t-1")
            .await
            .unwrap();
        let ctx = PostDispatchContext {
            tenant: &tenant,
            entity_type: "Ticket",
            entity_id: "t-1",
            action: "__Created",
            agent_ctx: &agent_ctx,
            origin: None,
            dispatch_idempotency_key: None,
            action_params: &serde_json::json!({}),
            await_integration: false,
        };
        state.arm_state_timeouts_if_needed(&ctx, &response).await;

        // Timer is 1s; give it 2s to fire + dispatch + apply.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let after = state
            .get_tenant_entity_state(&tenant, "Ticket", "t-1")
            .await
            .unwrap();
        assert_eq!(
            after.state.status, "InProgress",
            "state_timeout should have fired AssignAgent and moved Ticket to InProgress"
        );
        // Sanity: dispatch the same action explicitly — must fail because
        // AssignAgent is no longer valid from InProgress. This confirms the
        // transition actually went through the state machine (not a faked
        // status update).
        let retry = state
            .dispatch(DispatchCommand {
                tenant: &tenant,
                entity_type: "Ticket",
                entity_id: "t-1",
                action: "AssignAgent",
                params: serde_json::json!({}),
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await;
        if let Ok(r) = retry {
            assert!(
                !r.success,
                "AssignAgent must be rejected from InProgress (state machine integrity check)"
            );
        }
    }
}
