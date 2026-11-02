//! A state timer nothing names a caller for stops, and its record says so.
//!
//! A timer whose chain reaches a move that binds its caller
//! (`authenticated_subject`) can run that move only as someone. A timer that
//! acts for no one is refused there every time it fires, so repeating it does
//! nothing but repeat the refusal, and the record it was meant to move sits
//! still with nothing to say why. So the first such refusal stops the timer,
//! wherever in its chain it comes, an integration's answer's reaction included.
//! The timer's row goes, and a kernel event sets the record's
//! `timer_needs_person` to the refused action. It comes back with the record
//! on any read, and one log line gives a monitor something to act on.
//!
//! The next move that names someone clears it and arms the timer as them, as
//! any move finding a record with no timer does. Until then no arming of this
//! timer that acts for no one fires, whichever path armed it
//! (`fire_state_timer`).
//!
//! Only a refusal of the timer's own identity counts. A person's timer, or a
//! rule that declares a principal of its own, is refused for a reason a person
//! moving the record would not change. A rule Cedar refuses is not this
//! refusal either: a policy can refuse a service on purpose while the same
//! fire's other reactions do their work.

use temper_authz::TimerRecord;
use temper_runtime::tenant::TenantId;

use super::DispatchError;
use super::authenticated_params::refused_for_no_caller;
use crate::entity_actor::TimerStop;
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// The log line a monitor or the line can open an incident from.
const STOPPED: &str = "state timer stopped: nothing names its caller";

/// The record whose state timer `agent`'s move belongs to, when the move runs
/// as that timer's own identity.
fn timer_it_runs_for(agent: &AgentContext) -> Option<TimerRecord> {
    let Some(context) = agent.security_ctx.as_ref() else {
        return agent.fires_state_timer.as_ref().map(|fire| TimerRecord {
            entity_type: fire.entity_type.clone(),
            entity_id: fire.entity_id.clone(),
        });
    };
    let arm = context.cause.as_ref()?.timer.as_ref()?;
    if arm.armed_by != context.principal.id {
        return None;
    }
    arm.state_timer_of.clone()
}

impl ServerState {
    /// Called with `error`, the refusal of `action`, made as `agent`: stop the
    /// state timer behind it if nothing names who that timer fires as.
    pub(super) async fn stop_timer_no_one_can_run(
        &self,
        tenant: &TenantId,
        action: &str,
        agent: &AgentContext,
        error: &DispatchError,
    ) {
        if !refused_for_no_caller(error) || agent.caller().is_some() {
            return;
        }
        let Some(TimerRecord {
            entity_type,
            entity_id,
        }) = timer_it_runs_for(agent)
        else {
            return;
        };
        // Decided against the record as read, and again whenever a move lands
        // before the stop is written: that move may name someone. Each pass
        // follows a newer event, so this ends once the record stops moving.
        loop {
            let armed = self.armed_state_timer(tenant, &entity_type, &entity_id);
            if armed.is_some_and(|armed| armed.agent_ctx.caller().is_some()) {
                // Its next arming already names someone.
                return;
            }
            let current = match self
                .get_tenant_entity_state(tenant, &entity_type, &entity_id)
                .await
            {
                Ok(current) => current.state,
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "record of a timer no one can run not readable; its next refusal stops it");
                    return;
                }
            };
            let setter = match self.journal_setter(tenant, &entity_type, &entity_id).await {
                Ok(setter) => setter,
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "record journal not readable; its timer's setter is not known yet");
                    None
                }
            };
            self.cancel_state_timer(tenant, &entity_type, &entity_id, current.sequence_nr)
                .await;
            if let Some(setter) = setter {
                // Armed for no one before a move named someone, which kept that
                // timer: it runs as them from here.
                self.reconcile_state_timers(
                    tenant,
                    &entity_type,
                    &entity_id,
                    &current,
                    Some(&setter),
                )
                .await;
                return;
            }
            let said = self
                .say_timer_needs_person(
                    tenant,
                    &entity_type,
                    &entity_id,
                    action,
                    current.sequence_nr,
                )
                .await;
            if said != Some(TimerStop::Moved) {
                return;
            }
        }
    }

    /// Leave `action` on the record as what its timer stopped for, decided
    /// with the record at journal sequence `checked`, and the log line. A
    /// record that moved since is left as it is, with no line
    /// ([`TimerStop::Moved`]). A write that fails leaves only the line (`None`):
    /// the timer's next arming, at the next boot at the latest, fires and asks
    /// again.
    pub(super) async fn say_timer_needs_person(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        checked: u64,
    ) -> Option<TimerStop> {
        let recorded = self
            .record_timer_needs_person(tenant, entity_type, entity_id, action, checked)
            .await;
        if recorded == Ok(TimerStop::Moved) {
            return Some(TimerStop::Moved);
        }
        tracing::warn!(
            tenant = %tenant,
            entity_type,
            entity_id,
            action,
            recorded = recorded.is_ok(),
            error = recorded.as_ref().err(),
            "{STOPPED}"
        );
        recorded.ok()
    }
}
