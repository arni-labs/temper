//! What a move's sender is told about a reaction of it that was not made.
//!
//! The sender may not be allowed to read the reaction's target, so the
//! sentence names only what the rule itself names (the target's type, the id
//! the rule resolved and the move) and the kind of reason. It never carries
//! the target's state, its values, a guard's words or a policy's: those are in
//! the kernel's log line and in the source record's observe event.

use crate::entity_actor::EntityResponse;
use temper_jit::table::Refusal;

/// The kind of reason a reaction's move was not made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    /// The target does not exist, and the move does not make one.
    Missing,
    /// The target is not in a state the move starts from.
    State,
    /// A guard of the move did not hold on the target.
    Guard,
    /// The app's Cedar refused the move on the target.
    Withheld,
    /// The move could not be sent, or failed some other way.
    Other,
}

impl Why {
    /// Why the target refused the move it was sent. `target_existed`: whether
    /// the target was there when the reaction read it.
    pub(super) fn of(response: &EntityResponse, target_existed: bool) -> Self {
        match &response.refusal {
            _ if response.state.is_tombstoned() => Self::Missing,
            Some(Refusal::InvalidState { .. }) if !target_existed => Self::Missing,
            Some(Refusal::InvalidState { .. }) => Self::State,
            Some(Refusal::GuardFailed { .. }) => Self::Guard,
            _ => Self::Other,
        }
    }

    /// The `reason` label of `temper_reaction_refused_total`.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::State => "state",
            Self::Guard => "guard",
            Self::Withheld => "withheld",
            Self::Other => "other",
        }
    }
}

/// The sentence for a move the target did not take.
pub(super) fn not_made(entity_type: &str, entity_id: &str, action: &str, why: Why) -> String {
    let target = format!("{entity_type} '{entity_id}'");
    match why {
        Why::Missing => format!("{target} does not exist, and {action} does not make one"),
        Why::State => {
            format!("{target} did not take {action}: not possible from the state it is in")
        }
        Why::Guard => {
            format!("{target} did not take {action}: a condition of the move did not hold")
        }
        Why::Withheld => format!("{target} did not take {action}: not permitted"),
        Why::Other => format!("{target} did not take {action}"),
    }
}

/// The sentence for a trigger that found no id to send its move to.
pub(super) fn unresolved(entity_type: &str, action: &str) -> String {
    format!("no {entity_type} to send {action} to: the trigger found no id")
}

/// The sentence for a reaction stopped because its chain is a loop.
pub(super) const LOOP: &str = "not run: it would repeat a step its chain is already in";
