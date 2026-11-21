//! A background call whose module answers `retry` is made again.
//!
//! A module that cannot answer yet (a service it calls is down, something it
//! waits for is not ready) answers `{"retry": true, "success": false,
//! "error": <why>}`. Nothing moves. The call is made again with the same
//! trigger, params and idempotency key, and the next `attempt`: first after
//! [`FIRST_RETRY_WAIT`], then twice as long after each try, up to
//! [`LAST_RETRY_WAIT`]. There is no cap on tries; the deadline bounds them.
//!
//! - The deadline is the trigger's, as for any call (`timeout_secs`, else the
//!   engine's limit), counted from the first try, made at the move's event:
//!   the same instant its deadline row is due. When it passes, the
//!   trigger's `on_failure` is sent, once, with the module's last reason; a
//!   trigger with none fails as a failed call does.
//! - Before each try after the first, the record is read: if it has moved on
//!   from the event of the move that set the call off, the call is dropped
//!   (logged), and nothing is sent. Its deadline row goes when the call ends,
//!   as for any call.
//!
//! The call stays owed while it waits: the background task that makes it
//! holds the move's follow-on row (`follow_ons`), whose due time is the
//! move's event. Tries and deadline are both counted from that event, so a
//! restart resumes the call where its schedule is: the next process makes
//! the try due last, with that try's `attempt`, and the same deadline, which
//! its deadline row and its tries share. The call is answered once: by its
//! module, or by its deadline.

use std::time::Duration;

use temper_runtime::scheduler::sim_now;
use temper_wasm::WasmInvocationResult;

use super::boxed::handle_wasm_failure_boxed;
use super::{WasmAuthzGate, WasmDispatchCtx};
use crate::entity_actor::{EntityResponse, EntityState};
use crate::state::ServerState;
use crate::state::dispatch::call_deadlines::call_deadline;
use crate::state::dispatch::owed_writes::{FIRST_RETRY_WAIT, LAST_RETRY_WAIT};

/// How one try of a call ended.
pub(in crate::state::dispatch) enum Called {
    /// The module answered, or the call failed: it has ended.
    Answered(Option<Box<EntityResponse>>),
    /// The module answered `retry`, for this reason.
    Again(String),
}

impl Called {
    /// A try that ended the call with `response`.
    pub(super) fn answered(response: Option<EntityResponse>) -> Self {
        Self::Answered(response.map(Box::new))
    }
}

/// How a call ended.
pub(super) enum CallEnded {
    /// The module answered, the call failed or was given up on, or its
    /// record moved on: the call is over, and its deadline row can go.
    Over(Option<Box<EntityResponse>>),
    /// A resumed call whose deadline passed before this process could try it
    /// again: its stored deadline row answers it, so it stays.
    LeftToDeadlineRow,
}

/// The deadline a try runs under: it may run only until `due`, and one the
/// deadline stops is given up with `given_up`, the module's last retry
/// reason, when there was one.
#[derive(Clone, Copy)]
pub(super) struct TryDeadline<'a> {
    pub(super) due: chrono::DateTime<chrono::Utc>,
    pub(super) given_up: Option<&'a str>,
}

impl TryDeadline<'_> {
    /// How long a try that starts now may run: what its module may take,
    /// cut to what is left before the deadline.
    pub(super) fn time_left(&self, limit: Duration) -> Duration {
        (self.due - sim_now())
            .to_std()
            .unwrap_or(Duration::ZERO)
            .min(limit)
    }

    /// Whether a try stopped by its time limit was stopped by the deadline.
    pub(super) fn passed(&self) -> bool {
        // The engine stops a module by the wall clock, a little after its
        // limit: a second's slack.
        sim_now() + chrono::Duration::seconds(1) >= self.due
    }
}

/// Whether the record is still at the event of the move that set a call off.
enum Still {
    There,
    MovedOn,
    /// The record could not be read: no try runs on fields it cannot confirm.
    Unread,
}

fn given_up_after(module: &str, tries: u32, reason: &str) -> String {
    format!(
        "{module} asked to be called again until its deadline passed, after {tries} tries: {reason}"
    )
}

impl ServerState {
    /// Make the call `integration` names until its module answers, its
    /// deadline passes or its record moves on.
    ///
    /// A fresh call's first try is now, at attempt 1, and its deadline counts
    /// from its move's event, where its deadline row is due; a move that
    /// wrote no event leaves an older one, so a deadline that would already
    /// have passed counts from now. A resumed call (a boot sends it again)
    /// resumes at the try its schedule is at, under its stored deadline row's
    /// due time, so an install that changed the trigger's timeout since does
    /// not move it; it checks its record is still at its move before every
    /// try, the first too. Every try runs only until the deadline, and the
    /// next waits from the module's answer.
    pub(super) async fn call_until_answered(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        entity_state: &EntityState,
        action_params: &serde_json::Value,
        base_gate: &std::sync::Arc<dyn WasmAuthzGate>,
    ) -> Result<CallEnded, String> {
        let timeout = call_deadline(integration);
        let timeout_at = |from: chrono::DateTime<chrono::Utc>| {
            from + chrono::Duration::from_std(timeout).unwrap_or(chrono::Duration::MAX)
        };
        let now = sim_now();
        let resumed = ctx.agent_ctx.resumed_calls;
        let moved_at = entity_state.last_event.last_event_at;
        let stored = if resumed {
            self.stored_deadline(ctx).await
        } else {
            None
        };
        let (mut tries, due) = if resumed {
            let first = moved_at.unwrap_or(now);
            (
                Tries::resumed(first),
                stored.unwrap_or_else(|| timeout_at(first)),
            )
        } else {
            let from = moved_at.filter(|at| timeout_at(*at) > now).unwrap_or(now);
            (Tries::starting(now), timeout_at(from))
        };
        let module_name = integration.module.as_deref().unwrap_or_default();
        // Only a call with a deadline row (its trigger names an on_failure)
        // has a deadline its tries must stop at. One without is made again
        // after a restart however long the process was down, and its
        // retries are given up on at its timeout, as a failed call.
        let has_deadline_row = ctx.agent_ctx.ends_call.is_some();
        let mut last_reason: Option<String> = None;
        let mut check = resumed;
        loop {
            if check {
                match self.still_at(ctx, entity_state).await {
                    Still::There => {}
                    Still::MovedOn => {
                        tracing::info!(
                            tenant = %ctx.entity_ref.tenant,
                            entity_type = ctx.entity_ref.entity_type,
                            entity_id = ctx.entity_ref.entity_id,
                            integration = %integration.name,
                            attempt = tries.attempt,
                            "the record moved on from the move that set this call off; the call is dropped"
                        );
                        return Ok(CallEnded::Over(None));
                    }
                    Still::Unread => {
                        if sim_now() < due {
                            wait_until(due, FIRST_RETRY_WAIT).await;
                            continue;
                        }
                    }
                }
            }
            check = true;
            if sim_now() >= due && (has_deadline_row || last_reason.is_some()) {
                let error = match &last_reason {
                    Some(reason) => given_up_after(module_name, tries.attempt - 1, reason),
                    // Its stored deadline row answers a resumed call whose
                    // deadline passed while no process could make it.
                    None if stored.is_some() => return Ok(CallEnded::LeftToDeadlineRow),
                    None => format!(
                        "{module_name}'s call passed its deadline before it could be made again"
                    ),
                };
                return handle_wasm_failure_boxed(
                    self,
                    ctx,
                    &integration.name,
                    module_name,
                    &integration.on_failure,
                    error,
                    0,
                )
                .await
                .map(|response| CallEnded::Over(response.map(Box::new)));
            }
            let given_up = last_reason
                .as_deref()
                .map(|reason| given_up_after(module_name, tries.attempt, reason));
            let try_ctx = WasmDispatchCtx {
                attempt: tries.attempt,
                deadline: has_deadline_row.then_some(TryDeadline {
                    due,
                    given_up: given_up.as_deref(),
                }),
                ..*ctx
            };
            match self
                .dispatch_single_integration(
                    &try_ctx,
                    integration,
                    entity_state,
                    action_params,
                    base_gate,
                )
                .await?
            {
                Called::Answered(response) => {
                    return Ok(CallEnded::Over(response));
                }
                Called::Again(reason) => last_reason = Some(reason),
            }
            // The next try waits from this answer, however long the module took.
            let wait = tries.wait;
            tries.next();
            wait_until(due, wait).await;
        }
    }

    /// The due time of the deadline row of the call `ctx` makes, as stored:
    /// None when it has none or it cannot be read.
    async fn stored_deadline(
        &self,
        ctx: &WasmDispatchCtx<'_>,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        let call = ctx.agent_ctx.ends_call.as_ref()?;
        let (store, _) = self.event_journal()?;
        let rows = store
            .list_entity_scheduled_actions(&call.tenant, &call.entity_type, &call.entity_id)
            .await
            .ok()?;
        let row = rows.into_iter().find(|row| row.arm_id == call.arm_id)?;
        chrono::DateTime::parse_from_rfc3339(&row.due_at)
            .ok()
            .map(|at| at.with_timezone(&chrono::Utc))
    }

    /// Record a try the module answered `retry`, and return its reason.
    pub(super) async fn note_retry(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        module_name: &str,
        result: &WasmInvocationResult,
    ) -> String {
        let reason = result
            .error
            .clone()
            .unwrap_or_else(|| "the module asked to be called again".to_string());
        tracing::info!(
            tenant = %ctx.entity_ref.tenant,
            entity_type = ctx.entity_ref.entity_type,
            entity_id = ctx.entity_ref.entity_id,
            module = module_name,
            attempt = ctx.attempt,
            reason = %reason,
            "the module answered retry; the call is made again later"
        );
        self.record_invocation(
            ctx.entity_ref,
            module_name,
            ctx.action,
            None,
            false,
            Some(format!("retry: {reason}")),
            result.duration_ms,
            None,
        )
        .await;
        reason
    }

    /// Whether the record is still at the event of the move that set the call
    /// off, as `entity_state` holds it (a resumed call's is at its move's
    /// sequence, `follow_ons`).
    async fn still_at(&self, ctx: &WasmDispatchCtx<'_>, entity_state: &EntityState) -> Still {
        match self
            .get_tenant_entity_state(
                ctx.entity_ref.tenant,
                ctx.entity_ref.entity_type,
                ctx.entity_ref.entity_id,
            )
            .await
        {
            Ok(now) if now.state.event_sequence() == entity_state.event_sequence() => Still::There,
            Ok(_) => Still::MovedOn,
            Err(_) => Still::Unread,
        }
    }
}

/// Where a call's tries are: the first at the move's event, the next
/// [`FIRST_RETRY_WAIT`] later, then twice as long after each, up to
/// [`LAST_RETRY_WAIT`].
struct Tries {
    /// The try due at `at`, from 1.
    attempt: u32,
    /// When it is due.
    at: chrono::DateTime<chrono::Utc>,
    /// How long after it the next is due.
    wait: Duration,
}

/// Wait `wait`, or until `due` if that comes first.
async fn wait_until(due: chrono::DateTime<chrono::Utc>, wait: Duration) {
    let left = (due - sim_now()).to_std().unwrap_or(Duration::ZERO);
    let wait = wait.min(left);
    if !wait.is_zero() {
        tokio::time::sleep(wait).await; // determinism-ok: retry backoff on the paused clock in simulation
    }
}

impl Tries {
    /// A call's first try, made at `now`.
    fn starting(now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            attempt: 1,
            at: now,
            wait: FIRST_RETRY_WAIT,
        }
    }

    /// The try due last for a call whose first try was at `first`: the first
    /// for a call just made, a later one for a call a restart resumes.
    fn resumed(first: chrono::DateTime<chrono::Utc>) -> Self {
        let mut tries = Self {
            attempt: 1,
            at: first,
            wait: FIRST_RETRY_WAIT,
        };
        let now = sim_now();
        while tries.after() <= now {
            tries.next();
        }
        tries
    }

    /// When the try after this one is due.
    fn after(&self) -> chrono::DateTime<chrono::Utc> {
        let wait = chrono::Duration::from_std(self.wait).unwrap_or(chrono::Duration::MAX);
        self.at
            .checked_add_signed(wait)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC)
    }

    /// Move on to the next try.
    fn next(&mut self) {
        self.at = self.after();
        self.attempt = self.attempt.saturating_add(1);
        self.wait = (self.wait * 2).min(LAST_RETRY_WAIT);
    }
}
