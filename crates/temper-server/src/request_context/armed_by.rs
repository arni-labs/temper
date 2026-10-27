//! The identity a durable timer row keeps, so a timer re-armed after a restart
//! fires as whoever armed it. State timer rows and scheduled-action rows keep
//! it in the same form.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_authz::SecurityContext;

use super::AgentContext;

/// The identity part of an [`AgentContext`] that a timer row keeps. Runtime
/// budgets and the idempotency key are not kept: a timer fires as a new
/// background action.
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
}

/// `agent`'s identity as a row keeps it (JSON).
pub(crate) fn encode_armed_by(agent: &AgentContext) -> Result<String, serde_json::Error> {
    serde_json::to_string(&ArmedBy {
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
    })
}

/// The identity a row keeps, as a context to fire with.
pub(crate) fn decode_armed_by(armed_by: &str) -> Result<AgentContext, serde_json::Error> {
    let armed_by: ArmedBy = serde_json::from_str(armed_by)?;
    Ok(AgentContext {
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
        ..AgentContext::default()
    })
}
