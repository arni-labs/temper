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
//!   engine's limit), counted from the first try. When it passes, the
//!   trigger's `on_failure` is sent, once, with the module's last reason; a
//!   trigger with none fails as a failed call does.
//! - Before each try after the first, the record is read: if it has moved on
//!   from the event of the move that set the call off, the call is dropped
//!   (logged), and nothing is sent. Its deadline row goes when the call ends,
//!   as for any call.
//!
//! The call stays owed while it waits: the background task that makes it
//! holds the move's follow-on row (`follow_ons`), so a restart makes the call
//! again from its first try.

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
        let due = sim_now() + call_deadline(integration);
        let mut wait = FIRST_RETRY_WAIT;
        let mut attempt: u32 = 1;
        loop {
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
            let left = (due - sim_now()).to_std().unwrap_or(Duration::ZERO);
            if !left.is_zero() {
                tokio::time::sleep(wait.min(left)).await; // determinism-ok: retry backoff on the paused clock in simulation
                wait = (wait * 2).min(LAST_RETRY_WAIT);
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
            attempt = attempt.saturating_add(1);
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
