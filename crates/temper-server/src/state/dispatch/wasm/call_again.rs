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

impl ServerState {
    /// Make the call `integration` names until its module answers, its
    /// deadline passes or its record moves on.
    pub(super) async fn call_until_answered(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        entity_state: &EntityState,
        action_params: &serde_json::Value,
        base_gate: &std::sync::Arc<dyn WasmAuthzGate>,
    ) -> Result<Option<EntityResponse>, String> {
        let first_try = entity_state
            .last_event
            .last_event_at
            .unwrap_or_else(sim_now);
        let due = first_try + call_deadline(integration);
        let mut tries = Tries::resumed(first_try);
        loop {
            let attempt = tries.attempt;
            let try_ctx = WasmDispatchCtx { attempt, ..*ctx };
            let reason = match self
                .dispatch_single_integration(
                    &try_ctx,
                    integration,
                    entity_state,
                    action_params,
                    base_gate,
                )
                .await?
            {
                Called::Answered(response) => return Ok(response.map(|response| *response)),
                Called::Again(reason) => reason,
            };
            tries.next();
            let wait = (tries.at.min(due) - sim_now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await; // determinism-ok: retry backoff on the paused clock in simulation
            }
            if !self.still_at(&try_ctx, entity_state).await {
                tracing::info!(
                    tenant = %ctx.entity_ref.tenant,
                    entity_type = ctx.entity_ref.entity_type,
                    entity_id = ctx.entity_ref.entity_id,
                    integration = %integration.name,
                    attempt,
                    "the record moved on from the move that set this call off; the call is dropped"
                );
                return Ok(None);
            }
            if sim_now() >= due {
                let module_name = integration.module.as_deref().unwrap_or_default();
                let error = format!(
                    "{module_name} asked to be called again until its deadline passed, after {attempt} tries: {reason}"
                );
                return handle_wasm_failure_boxed(
                    self,
                    &try_ctx,
                    &integration.name,
                    module_name,
                    &integration.on_failure,
                    error,
                    0,
                )
                .await;
            }
        }
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
    /// off. A record that cannot be read is taken as still there: the next try
    /// is made, and its answer is checked as any answer is.
    async fn still_at(&self, ctx: &WasmDispatchCtx<'_>, entity_state: &EntityState) -> bool {
        match self
            .get_tenant_entity_state(
                ctx.entity_ref.tenant,
                ctx.entity_ref.entity_type,
                ctx.entity_ref.entity_id,
            )
            .await
        {
            Ok(now) => now.state.event_sequence() == entity_state.event_sequence(),
            Err(_) => true,
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

impl Tries {
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
