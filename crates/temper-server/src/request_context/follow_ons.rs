//! The row a committed move's follow-ons are owed by.
//!
//! A move that commits can owe writes after its commit: the reactions its
//! action triggers, the children it spawns and its integrations' calls and
//! answers. Each runs after the commit, in this process, and a restart in
//! between used to lose it. A move that owes any writes, in its own commit,
//! a scheduled-action row that names them ([`OwedFollowOns`]), with the id
//! [`follow_arm_id`] of the key it commits under. The row is removed once
//! every follow-on has settled in this process (it landed, was refused, or
//! failed for good), and a boot sends the follow-ons of every row a previous
//! process left (`state::dispatch::follow_ons`). Each follow-on carries the
//! key derived from the move's key, as it did the first time, so one that had
//! landed is answered as a duplicate and applies once.

use serde::{Deserialize, Serialize};

use super::AgentContext;
use crate::entity_actor::EntityResponse;
use crate::entity_actor::effects::SpawnRequest;
use crate::trigger::ReactionRule;

/// What starts a follow-on row's arm id; the rest is the move's key. A
/// schedule's id starts with its event's sequence and a digit, so the two
/// never meet.
use temper_runtime::persistence::FOLLOW_ON_ARM_PREFIX as FOLLOW_ARM;

/// The arm id of the follow-on row of the move committed under `key`. A move
/// sent again under its key names the same row.
pub(crate) fn follow_arm_id(key: &str) -> String {
    format!("{FOLLOW_ARM}{key}")
}

/// Whether a scheduled-action row is a move's follow-on row.
pub(crate) fn is_follow_row(arm_id: &str) -> bool {
    temper_runtime::persistence::is_follow_on_arm(arm_id)
}

/// Whether a move that spawns `spawn_requests` and emits `custom_effects`
/// owes follow-ons; `reactions` says whether its caller runs its reactions.
/// The actor writes the row by this rule, and the dispatcher holds the row
/// by it, so the two agree on every move.
pub(crate) fn owes_follow_ons(
    spawn_requests: &[SpawnRequest],
    custom_effects: &[String],
    reactions: bool,
) -> bool {
    reactions || !spawn_requests.is_empty() || !custom_effects.is_empty()
}

/// Whether the committed move `response` answers wrote a follow-on row: it
/// succeeded, wrote an event, and owes follow-ons.
pub(crate) fn wrote_follow_row(response: &EntityResponse, reactions: bool) -> bool {
    response.success
        && response.unrecorded_at.is_none()
        && owes_follow_ons(
            &response.spawn_requests,
            &response.custom_effects,
            reactions,
        )
}

/// The follow-ons a move owes, as its row keeps them: what sending them
/// again after a restart needs. Kept as JSON in the row's `armed_by`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OwedFollowOns {
    /// Who made the move, with its key, as a scheduled row keeps them
    /// (`armed_by::encode_scheduled_armed_by`).
    pub(crate) caller: String,
    /// The journal sequence of the move's event.
    pub(crate) sequence: u64,
    /// The record's status after the move.
    pub(crate) status: String,
    /// The record's fields after the move.
    pub(crate) fields: serde_json::Value,
    /// The move's parameters.
    pub(crate) params: serde_json::Value,
    /// The reactions its action declared when it was sent; the status above
    /// decides which of them run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) reactions: Vec<ReactionRule>,
    /// The children it spawns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) spawn_requests: Vec<SpawnRequest>,
    /// The custom effects its integrations run on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) custom_effects: Vec<String>,
}

impl OwedFollowOns {
    /// The caller the follow-ons are sent as, read back from the row.
    pub(crate) fn caller(&self) -> Result<AgentContext, serde_json::Error> {
        super::armed_by::decode_armed_by(&self.caller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_follow_row_is_never_a_schedule_or_a_call_deadline() {
        let arm_id = follow_arm_id("dispatch:t:Job:1:Ask:abc");
        assert!(is_follow_row(&arm_id));
        assert!(!is_follow_row("12.0"));
        assert!(!is_follow_row("12.call:lookup"));
        assert_eq!(super::super::calls::called_integration(&arm_id), None);
    }
}
