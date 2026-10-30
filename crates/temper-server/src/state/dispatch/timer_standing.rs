//! A timer acts with the standing its setter has when it fires.
//!
//! A timer fires as whoever set it (`timer_cause`). Its move is the app's own,
//! the spec's consequence of the move into the timed state, so Cedar is not
//! asked about the move itself. It is asked about the setter: whether they
//! may still read the record, as the record is now and with the timer's cause
//! (`context.via == "timer"`). A setter who may read it has standing on it,
//! and the timer moves the record as before. A setter who may not has none,
//! and the move is not made: the fire ends as a final refusal, and it is
//! logged, counted and kept as an authorization trajectory with the setter
//! and Cedar's reason.
//!
//! A timer the kernel set as one of its own identities (a re-arm as the
//! state-timer service, a module's callback, a rule's declared service, an
//! internal caller) acts for no one, so it has no standing to lose and fires
//! as before ([`AgentContext::caller`]).

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::DispatchError;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

/// What a timer's fire may do, by its setter's standing on the record now.
pub(super) enum Standing {
    /// The setter may read the record, or the timer acts for no one: its move
    /// is made.
    Holds,
    /// The setter may no longer read the record: its move is not made.
    Lost {
        /// Cedar's reason.
        reason: String,
    },
}

/// One timer fire: the record it moves and the move it makes.
pub(super) struct TimerFire<'a> {
    pub(super) tenant: &'a TenantId,
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) action: &'a str,
}

impl ServerState {
    /// The standing of the setter of a timer whose fire is `agent` (its
    /// identity, marked with the timer's cause), on the record `fire` moves,
    /// as Cedar decides now. A lost standing is reported here. An error is a
    /// record that could not be read, which the fire tries again.
    pub(super) async fn timer_setter_standing(
        &self,
        agent: &AgentContext,
        fire: &TimerFire<'_>,
    ) -> Result<Standing, DispatchError> {
        let Some(setter) = agent.caller() else {
            return Ok(Standing::Holds);
        };
        let snapshot = self
            .load_authz_resource_snapshot(fire.tenant, fire.entity_type, fire.entity_id)
            .await?;
        let Err(denial) = self.authorize_with_context(
            setter,
            crate::odata::authz::READ_ACTION,
            fire.entity_type,
            &snapshot.resource_attrs,
            fire.tenant.as_str(),
        ) else {
            return Ok(Standing::Holds);
        };
        let reason = denial.to_string();
        let status = snapshot.current_state.state.status;
        tracing::warn!(
            tenant = %fire.tenant,
            entity_type = fire.entity_type,
            entity_id = fire.entity_id,
            action = fire.action,
            status = %status,
            setter = %setter.principal.id,
            setter_kind = ?setter.principal.kind,
            acting_for = ?setter.principal.acting_for,
            %reason,
            "timer fire withheld: its setter may not read the record now"
        );
        crate::runtime_metrics::record_timer_fire_withheld(
            fire.tenant.as_str(),
            fire.entity_type,
            fire.action,
        );
        let entry = TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: fire.tenant.to_string(),
            entity_type: fire.entity_type.to_string(),
            entity_id: fire.entity_id.to_string(),
            action: fire.action.to_string(),
            success: false,
            from_status: Some(status),
            to_status: None,
            error: Some(format!(
                "timer fire withheld: its setter may not read the record now: {reason}"
            )),
            agent_id: Some(setter.principal.id.clone()),
            session_id: agent.session_id.clone(),
            authz_denied: Some(true),
            denied_resource: Some(format!("{}:{}", fire.entity_type, fire.entity_id)),
            denied_module: None,
            source: Some(TrajectorySource::Authz),
            spec_governed: Some(true),
            agent_type: setter.principal.agent_type.clone(),
            request_body: Some(serde_json::to_value(&snapshot.resource_attrs).unwrap_or_default()),
            intent: agent.intent.clone(),
            matched_policy_ids: None,
            capture_seq: None,
        };
        if !self.enqueue_trajectory_entry(entry) {
            tracing::warn!("failed to enqueue withheld timer fire trajectory");
        }
        Ok(Standing::Lost { reason })
    }
}
