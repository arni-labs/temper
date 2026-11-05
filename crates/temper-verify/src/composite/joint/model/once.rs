//! Dispatches a record can take only once, of which the walk keeps one copy
//! in flight.
//!
//! An action is taken once when each of its transitions leaves to a status
//! from which no transition of its type (any action, any guard) leads back
//! to a status the action is allowed from. Of identical dispatches of it to
//! one record (the same in every field: parameters, sender `by`, the
//! reaction `via` and chain that sent it, so Cedar answers each alike), at
//! most one is ever taken, and the walk keeps one copy, dropping a second
//! as it is sent.
//!
//! Why one is enough, both ways. A run to the walk: the walk holds a copy
//! while the run holds any. The run's move on the copy it takes, or on the
//! last to leave, is the walk's move on its one: they are alike, and the
//! states differ only in copies of it. Its move on any other copy is a
//! refusal, which changes no record, and the walk waits. So a copy refused
//! while its record still takes the action (a guard on another field, a
//! constraint, Cedar), or refused outside the action's statuses before the
//! record comes back to them, leaves the walk its copy to land later. The
//! walk to a run: the run makes the walk's moves and keeps every copy the
//! walk dropped. Where the walk's copy is taken the record never takes
//! another; where it is refused, the run refuses all of them there, in the
//! same state. So each walked state, emptied, is one the run reaches, and a
//! rule read where nothing is in flight finds the same. Dropping the copy,
//! rather than holding back the move that sent it, keeps the move and the
//! rest of what it does.
//!
//! Only a dispatch the dispatch path has read is kept once: a read turns
//! one copy into another dispatch while its twin waits, so two unread
//! copies are not one.
//!
//! Identical copies come from separate commits: one commit sends each
//! reaction once, and two of its reactions differ in `via`. A record that
//! can be put in place of another (several records of a type, and a
//! reaction that makes one) starts over at its first status, so its type's
//! actions are not taken once there. The drop check names every refused
//! landing, so its model keeps every copy (`drops.rs`).

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::TargetResolver;

use super::super::{Dispatch, JointState};
use super::JointModel;
use super::copies::base;
use crate::model::TemperModel;

/// The actions (entity, action) of `models` taken once.
pub(super) fn taken_once(models: &BTreeMap<String, TemperModel>) -> BTreeSet<(String, String)> {
    let mut once = BTreeSet::new();
    for (entity, model) in models {
        let names: BTreeSet<&str> = model.transitions.iter().map(|t| t.name.as_str()).collect();
        for name in names {
            let ways: Vec<_> = model
                .transitions
                .iter()
                .filter(|t| t.name == name)
                .collect();
            let from: BTreeSet<_> = ways.iter().flat_map(|t| t.from_states.iter()).collect();
            let leaves = ways.iter().all(|t| {
                !t.from_states.is_empty() && t.to_state.is_some_and(|to| !from.contains(&to))
            });
            let back = |start| {
                let mut seen = vec![start];
                let mut at = 0;
                while let Some(&now) = seen.get(at) {
                    at += 1;
                    if from.contains(&now) {
                        return true;
                    }
                    for t in &model.transitions {
                        if t.from_states.is_empty() || t.from_states.contains(&now) {
                            let to = t.to_state.unwrap_or(now);
                            if !seen.contains(&to) {
                                seen.push(to);
                            }
                        }
                    }
                }
                false
            };
            if leaves && !ways.iter().filter_map(|t| t.to_state).any(back) {
                once.insert((entity.clone(), name.to_string()));
            }
        }
    }
    once
}

impl JointModel {
    /// Whether one copy of `dispatch` in flight is enough: read, and of an
    /// action taken once.
    fn once(&self, dispatch: &Dispatch) -> bool {
        dispatch.read
            && dispatch.unread_guard.is_none()
            && self
                .taken_once
                .contains(&(base(&dispatch.to).to_string(), dispatch.action.to_string()))
    }

    /// Whether `state` holds a second copy of a dispatch taken once. Copies
    /// are the same in every field.
    pub(super) fn holds_a_second_copy(&self, state: &JointState) -> bool {
        state
            .inflight
            .windows(2)
            .any(|pair| pair[0] == pair[1] && self.once(&pair[0]))
    }

    /// `state` with one copy of each dispatch taken once.
    pub(super) fn one_copy(&self, state: &mut JointState) {
        if self.holds_a_second_copy(state) {
            state
                .inflight
                .dedup_by(|later, kept| later == kept && self.once(kept));
        }
    }

    /// Several records of a type, and a reaction that makes one: a record
    /// put in place of another starts over, so its type takes nothing once.
    pub(super) fn several_take_again(&mut self) {
        let made: BTreeSet<&str> = self
            .reactions
            .iter()
            .filter(|r| {
                r.spawn.is_some()
                    || matches!(
                        r.resolve,
                        Some(TargetResolver::Create | TargetResolver::CreateIfMissing { .. })
                    )
            })
            .map(|r| base(&r.to))
            .collect();
        self.taken_once
            .retain(|(entity, _)| !made.contains(entity.as_str()));
    }
}
