//! The identity a durable timer row keeps, so a timer re-armed after a restart
//! fires as whoever armed it, with what they could read when they set it.
//! State timer rows and scheduled-action rows keep it in the same form.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_authz::SecurityContext;

use super::AgentContext;
use crate::idempotency::IdempotencyKey;

/// The identity part of an [`AgentContext`] that a timer row keeps. Runtime
/// budgets and the idempotency key are kept only for scheduled follow-ons.
/// State timers fire as new background actions.
#[derive(Serialize, Deserialize)]
struct ArmedBy {
    security_ctx: Option<SecurityContext>,
    agent_id: Option<String>,
    session_id: Option<String>,
    agent_type: Option<String>,
    intent: Option<String>,
    trace_id: Option<String>,
    parent_span_id: Option<String>,
    workflow_root_entity_type: Option<String>,
    workflow_root_entity_id: Option<String>,
    workflow_run_id: Option<String>,
    #[serde(default)]
    observation_metadata: BTreeMap<String, String>,
    /// Absent from rows written before timers kept it, and ignored by a
    /// binary that predates it: such a timer fires as it always did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_when_set: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scheduled_follow_on: Option<ScheduledFollowOn>,
}

/// The causal context of a scheduled action, already separated from its parent.
#[derive(Serialize, Deserialize)]
struct ScheduledFollowOn {
    callback_hops: u32,
    idempotency_key: Option<String>,
}

/// `agent`'s identity as a row keeps it (JSON).
pub(crate) fn encode_armed_by(agent: &AgentContext) -> Result<String, serde_json::Error> {
    serde_json::to_string(&armed_by(agent))
}

/// A scheduled follow-on's identity, lineage and derived key as its row keeps them.
pub(crate) fn encode_scheduled_armed_by(agent: &AgentContext) -> Result<String, serde_json::Error> {
    let mut armed = armed_by(agent);
    armed.scheduled_follow_on = Some(ScheduledFollowOn {
        callback_hops: agent.callback_hops,
        idempotency_key: agent
            .idempotency_key
            .as_ref()
            .map(|key| key.as_str().to_string()),
    });
    serde_json::to_string(&armed)
}

fn armed_by(agent: &AgentContext) -> ArmedBy {
    ArmedBy {
        security_ctx: agent.security_ctx.clone(),
        agent_id: agent.agent_id.clone(),
        session_id: agent.session_id.clone(),
        agent_type: agent.agent_type.clone(),
        intent: agent.intent.clone(),
        trace_id: agent.trace_id.clone(),
        parent_span_id: agent.parent_span_id.clone(),
        workflow_root_entity_type: agent.workflow_root_entity_type.clone(),
        workflow_root_entity_id: agent.workflow_root_entity_id.clone(),
        workflow_run_id: agent.workflow_run_id.clone(),
        observation_metadata: agent.observation_metadata.clone(),
        read_when_set: agent.read_when_set,
        scheduled_follow_on: None,
    }
}

/// The identity a row keeps, as a context to fire with.
pub(crate) fn decode_armed_by(armed_by: &str) -> Result<AgentContext, serde_json::Error> {
    let armed_by: ArmedBy = serde_json::from_str(armed_by)?;
    let follow_on = armed_by.scheduled_follow_on;
    Ok(AgentContext {
        callback_hops: follow_on.as_ref().map_or(0, |follow| follow.callback_hops),
        idempotency_key: follow_on
            .and_then(|follow| follow.idempotency_key)
            .map(IdempotencyKey::FollowUp),
        security_ctx: armed_by.security_ctx,
        agent_id: armed_by.agent_id,
        session_id: armed_by.session_id,
        agent_type: armed_by.agent_type,
        intent: armed_by.intent,
        trace_id: armed_by.trace_id,
        parent_span_id: armed_by.parent_span_id,
        workflow_root_entity_type: armed_by.workflow_root_entity_type,
        workflow_root_entity_id: armed_by.workflow_root_entity_id,
        workflow_run_id: armed_by.workflow_run_id,
        observation_metadata: armed_by.observation_metadata,
        read_when_set: armed_by.read_when_set,
        ..AgentContext::default()
    })
}
