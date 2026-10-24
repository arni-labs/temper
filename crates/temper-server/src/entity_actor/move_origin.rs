//! Who made a move that no caller asked for directly, and what led to it.
//!
//! A state timer's move (ADR-0049) is the app's own rule acting: the spec
//! declares it, and the move that entered the timed state is what Cedar
//! judged. The record's history therefore names the state timer as the one
//! that made the move, and keeps the move that armed it as its cause, rather
//! than reading as if the person whose move armed the timer had made it.

use serde::{Deserialize, Serialize};

/// The agent id a state timer's move is recorded under, in trajectories,
/// change notifications and the record's history. Named like the other
/// runtime services (`service:<name>`, see `AgentContext::for_service`).
pub const STATE_TIMER_ACTOR: &str = "service:state-timer";

/// The move that led the runtime to make another one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveCause {
    /// The agent id of the dispatch that made the causing move, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// The causing move's action. Absent for a timer armed before causes
    /// were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// The record's journal sequence right after the causing move.
    pub sequence_nr: u64,
}

/// Who made a move that no caller asked for directly, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveOrigin {
    /// The runtime part that made the move, e.g. [`STATE_TIMER_ACTOR`].
    pub actor: String,
    /// The move that led to it.
    pub caused_by: MoveCause,
}

impl MoveOrigin {
    /// The agent type the move is recorded under: the service name in
    /// `service:<name>`, as `AgentContext::for_service` names it.
    pub fn actor_type(&self) -> &str {
        self.actor.strip_prefix("service:").unwrap_or(&self.actor)
    }
}

/// The agent id and type a move's own records (trajectory, change
/// notification, outbound webhook) name: the runtime part that made the move
/// when `origin` is set, else the dispatch's agent. Authority-bearing uses
/// (Cedar, integrations, adapters) read `agent` itself, so a state timer's
/// move keeps the identity of whoever armed it there.
pub(crate) fn recorded_agent(
    origin: Option<&MoveOrigin>,
    agent: &crate::request_context::AgentContext,
) -> (Option<String>, Option<String>) {
    match origin {
        Some(origin) => (
            Some(origin.actor.clone()),
            Some(origin.actor_type().to_string()),
        ),
        None => (agent.agent_id.clone(), agent.agent_type.clone()),
    }
}
