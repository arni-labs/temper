//! Dispatches a record can take only once, of which the walk keeps one copy
//! in flight.
//!
//! An action is taken once when each of its transitions leaves to a status
//! from which no transition of its type (any action, any guard) leads back
//! to a status the action is allowed from. Then, of identical dispatches of
//! it to one record (the same in every field), at most one is taken: once
//! one is, the record can never take another, and each other copy lands
//! refused, which only takes it out of flight. So the walk keeps one copy
//! and drops a second as it is sent. A run with `k` copies goes where the
//! walk goes with one, landing its first copy that is taken as the walk
//! lands its one (both are in flight, on the same record), and refusing the
//! others in the same state the walk is in, which changes nothing it keeps;
//! the walk's one copy, refused, stands for the run refusing all of them at
//! once. So a rule, read where nothing is in flight, finds the same either
//! way. Dropping the copy, rather than holding back the move that sent it,
//! keeps the move and the rest of what it does.
//!
//! Identical copies come from separate commits: one commit sends each
//! reaction once, and two of its reactions differ in `via`. A record that
//! can be put in place of another (several records of a type, and a
//! reaction that makes one) starts over at its first status, so its type's
//! actions are not taken once there. The drop check names every refused
//! landing, so its model keeps every copy (`drops.rs`).

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::TargetResolver;

use super::super::JointState;
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
    /// Whether a dispatch of `entity`'s `action` is taken once.
    fn once(&self, entity: &str, action: &str) -> bool {
        self.taken_once
            .contains(&(base(entity).to_string(), action.to_string()))
    }

    /// Whether `state` holds a second copy of a dispatch taken once.
    pub(super) fn holds_a_second_copy(&self, state: &JointState) -> bool {
        state
            .inflight
            .windows(2)
            .any(|pair| pair[0] == pair[1] && self.once(&pair[0].to, &pair[0].action))
    }

    /// `state` with one copy of each dispatch taken once.
    pub(super) fn one_copy(&self, state: &mut JointState) {
        if self.holds_a_second_copy(state) {
            state
                .inflight
                .dedup_by(|later, kept| later == kept && self.once(&kept.to, &kept.action));
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
