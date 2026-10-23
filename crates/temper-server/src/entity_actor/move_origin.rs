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

/// The agent type a state timer's move is recorded under.
pub const STATE_TIMER_AGENT_TYPE: &str = "state-timer";

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

/// Record on `event` who made it and why, when the runtime made it itself.
/// A move a caller asked for keeps no actor or cause.
pub(crate) fn record_origin(event: &mut super::EntityEvent, origin: Option<&MoveOrigin>) {
    event.actor = origin.map(|origin| origin.actor.clone());
    event.caused_by = origin.map(|origin| origin.caused_by.clone());
}
