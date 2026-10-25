//! How a move came about, as the kernel dispatched it.
//!
//! A move is a direct request, a reaction a spec declares to another move, or
//! a timer's fire. The kernel keeps the true principal on every path (a hook's
//! reaction runs as the hook, acting for its owner) and tells Cedar the path
//! instead, so an app can tell a principal's own pick of a move from what the
//! spec makes follow it:
//!
//! | Cedar context key  | Type   | Set on                                     |
//! |--------------------|--------|--------------------------------------------|
//! | `via`              | String | every check: `"request"`, `"reaction"` or `"timer"` |
//! | `depth`            | Long   | every check: 0 for a request or a timer's fire, +1 per reaction hop |
//! | `rule`             | String | a reaction: the kernel's name for its rule, `"<Type>:<Action>:<trigger>"` for a spec's `[[action.triggers]]` |
//! | `causeAction`      | String | a reaction: the move that set it off       |
//! | `causeEntityType`  | String | a reaction: that move's entity type        |
//! | `causeEntityId`    | String | a reaction: that move's entity id          |
//! | `armedAction`      | String | `via == "timer"`: the action that armed the timer, when known |
//! | `armedBy`          | String | `via == "timer"`: the arming principal's id |
//!
//! The kernel alone sets these keys: a record field or another context
//! attribute of the same name never reaches Cedar under them. A kernel that
//! predates them sets none, so a policy reads them behind `context has`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Cedar context keys a [`MoveCause`] sets. Only the kernel sets them.
pub(crate) const MOVE_CAUSE_CONTEXT_KEYS: [&str; 8] = [
    "via",
    "depth",
    "rule",
    "causeAction",
    "causeEntityType",
    "causeEntityId",
    "armedAction",
    "armedBy",
];

/// How a move came about when it was not a direct request: a reaction to
/// another move, or a timer's fire. No cause means a direct request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveCause {
    /// The reaction that made the move. None for a timer's own fire.
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub reaction: Option<ReactionHop>,
    /// Reaction hops from the chain's first move: 1 for a reaction to it, 2
    /// for a reaction to that reaction, and 0 for a timer's fire, which starts
    /// a chain of its own.
    pub depth: u32,
    /// The timer whose fire started this chain, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<TimerArm>,
}

/// The rule that made a reaction and the move that set it off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionHop {
    /// The kernel's name for the reaction rule: `"<Type>:<Action>:<trigger>"`
    /// for a trigger a spec declares on `<Type>.<Action>`.
    pub rule: String,
    /// The move that set the reaction off.
    pub action: String,
    /// That move's entity type.
    pub entity_type: String,
    /// That move's entity id.
    pub entity_id: String,
}

/// Who armed a timer, and with which move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerArm {
    /// The move that armed the timer: the record's move that entered its timed
    /// state or reset its clock. None when the kernel re-armed a timer on a
    /// record whose journal it no longer holds in memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub armed_action: Option<String>,
    /// The id of the principal that armed it, whom the timer fires as.
    pub armed_by: String,
}

impl MoveCause {
    /// A timer's own fire, armed as `arm` says.
    pub fn timer_fire(arm: TimerArm) -> Self {
        Self {
            reaction: None,
            depth: 0,
            timer: Some(arm),
        }
    }

    /// The reaction `hop` makes, set off by a move whose own cause was
    /// `source`: one hop deeper, in the same timer's chain if any.
    pub fn reaction(hop: ReactionHop, source: Option<&MoveCause>) -> Self {
        let source_depth = source.filter(|cause| cause.reaction.is_some());
        Self {
            reaction: Some(hop),
            depth: source_depth.map_or(0, |cause| cause.depth) + 1,
            timer: source.and_then(|cause| cause.timer.clone()),
        }
    }

    /// The `via` Cedar sees: `"timer"` within a timer's chain, else
    /// `"reaction"`.
    pub fn via(&self) -> &'static str {
        if self.timer.is_some() {
            "timer"
        } else {
            "reaction"
        }
    }
}

/// The Cedar context a move made with `cause` is checked with, as JSON
/// strings and a number (a Cedar Long): `via` and `depth` always, and the rest
/// as [`MoveCause`] has them. The one place the kernel turns a move's path into
/// Cedar input.
pub(crate) fn cause_context(cause: Option<&MoveCause>) -> Vec<(&'static str, Value)> {
    let string = |value: &str| Value::String(value.to_string());
    let Some(cause) = cause else {
        return vec![("via", string("request")), ("depth", Value::from(0))];
    };
    let mut context = vec![
        ("via", string(cause.via())),
        ("depth", Value::from(cause.depth)),
    ];
    if let Some(hop) = &cause.reaction {
        context.push(("rule", string(&hop.rule)));
        context.push(("causeAction", string(&hop.action)));
        context.push(("causeEntityType", string(&hop.entity_type)));
        context.push(("causeEntityId", string(&hop.entity_id)));
    }
    if let Some(timer) = &cause.timer {
        if let Some(action) = &timer.armed_action {
            context.push(("armedAction", string(action)));
        }
        context.push(("armedBy", string(&timer.armed_by)));
    }
    debug_assert!(
        context
            .iter()
            .all(|(key, _)| MOVE_CAUSE_CONTEXT_KEYS.contains(key))
    );
    context
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(rule: &str, action: &str) -> ReactionHop {
        ReactionHop {
            rule: rule.to_string(),
            action: action.to_string(),
            entity_type: "Direction".to_string(),
            entity_id: "d1".to_string(),
        }
    }

    fn arm() -> TimerArm {
        TimerArm {
            armed_action: Some("Park".to_string()),
            armed_by: "hook-1".to_string(),
        }
    }

    #[test]
    fn a_request_is_via_request_at_depth_zero() {
        assert_eq!(
            cause_context(None),
            vec![("via", Value::from("request")), ("depth", Value::from(0))]
        );
    }

    #[test]
    fn each_reaction_hop_is_one_deeper() {
        let first = MoveCause::reaction(hop("goes", "FindFix"), None);
        let second = MoveCause::reaction(hop("lands", "AutoGo"), Some(&first));
        assert_eq!((first.depth, second.depth), (1, 2));
        assert_eq!(second.via(), "reaction");
    }

    #[test]
    fn a_timers_chain_is_via_timer_and_keeps_its_arm() {
        let fire = MoveCause::timer_fire(arm());
        assert_eq!(fire.via(), "timer");
        assert_eq!(fire.depth, 0);
        let reaction = MoveCause::reaction(hop("follows_up", "Lapse"), Some(&fire));
        assert_eq!(reaction.depth, 1);
        assert_eq!(reaction.timer, Some(arm()));
        let context = cause_context(Some(&reaction));
        assert!(context.contains(&("via", Value::from("timer"))));
        assert!(context.contains(&("armedBy", Value::from("hook-1"))));
        assert!(context.contains(&("armedAction", Value::from("Park"))));
        assert!(context.contains(&("rule", Value::from("follows_up"))));
        assert!(context.contains(&("causeAction", Value::from("Lapse"))));
    }

    #[test]
    fn the_journal_shape_is_flat_and_round_trips() {
        let cause = MoveCause::reaction(hop("goes", "FindFix"), None);
        let json = serde_json::to_value(&cause).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"rule": "goes", "action": "FindFix",
                "entity_type": "Direction", "entity_id": "d1", "depth": 1})
        );
        assert_eq!(serde_json::from_value::<MoveCause>(json).unwrap(), cause);

        let fire = MoveCause::timer_fire(arm());
        let json = serde_json::to_value(&fire).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"depth": 0,
                "timer": {"armed_action": "Park", "armed_by": "hook-1"}})
        );
        assert_eq!(serde_json::from_value::<MoveCause>(json).unwrap(), fire);
    }
}
