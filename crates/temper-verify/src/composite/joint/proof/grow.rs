//! How many in flight a proof's walk holds: as many as fit its room, while
//! what it holds back can change what the rule reads.

use super::JointOutcome;
use super::model::{self, HeldBack, JointModel};

/// The walk `walk` makes, with as many in flight as fit: while it held back
/// a move for [`JointModel::in_flight`] that can change what a rule reads
/// ([`HeldBack::Reaching`]), saw every state, and a rule still holds, walked
/// again with twice as many. A walk that held back only moves that cannot
/// stops there: the cone's argument (`model/cone.rs`) says more in flight
/// reaches no value of what the rules read that it has not. Doubling keeps
/// the walks before the last under the last one's cost together, where one
/// more at a time walks once for each. A walk that does not fit keeps the
/// last that did, with the breaks it found sure; the bound is then the last
/// one walked whole.
pub(super) fn grown_walk(model: &JointModel, walk: impl Fn() -> model::Walk) -> model::Walk {
    let holds = |w: &model::Walk| {
        w.outcomes
            .iter()
            .any(|o| matches!(o, JointOutcome::Holds | JointOutcome::Vacuous { .. }))
    };
    let mut last = walk();
    while last.held_back == HeldBack::Reaching && !last.stopped && holds(&last) {
        let fitted = model.in_flight();
        model.hold(fitted * 2);
        let next = walk();
        if !next.stopped {
            last = next;
            continue;
        }
        model.hold(fitted);
        for (kept, found) in last.outcomes.iter_mut().zip(next.outcomes) {
            if matches!(found, JointOutcome::Broken { .. })
                && matches!(kept, JointOutcome::Holds | JointOutcome::Vacuous { .. })
            {
                *kept = found;
            }
        }
        break;
    }
    last
}

/// As [`grown_walk`], for a liveness rule's walk, growing also on moves
/// held back that cannot change what it reads when `aside_too`. One walk's
/// tables are given back before the next is made, and one that does not fit
/// is walked again with the last bound that did.
pub(super) fn grown_explored<'a>(
    model: &JointModel,
    aside_too: bool,
    walk: impl Fn() -> model::Explored<'a>,
) -> model::Explored<'a> {
    let grows =
        |held: HeldBack| held == HeldBack::Reaching || (aside_too && held != HeldBack::Nothing);
    let mut explored = walk();
    while grows(explored.held_back) && explored.stopped.is_none() {
        drop(explored);
        let fitted = model.in_flight();
        model.hold(fitted * 2);
        explored = walk();
        if explored.stopped.is_some() {
            drop(explored);
            model.hold(fitted);
            explored = walk();
            break;
        }
    }
    explored
}
