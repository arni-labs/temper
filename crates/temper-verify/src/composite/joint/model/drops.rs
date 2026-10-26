//! The drop check (ADR-0150) on the joint model: a reaction whose target
//! refuses it when it lands is dropped, and the runtime says nothing.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::fmt;

use temper_spec::automaton::{Automaton, TargetResolver};
use temper_spec::predicate::Truth;

use super::super::JointState;
use super::super::env::Records;
use super::commit::Arrival;
use super::step::{Move, remove};
use super::{Delivery, JointModel, Sending, tracked};

/// A reaction its target refused when it landed: the target was not in a
/// state its action is allowed from, or a guard or constraint said no.
/// Reactions that create their target, and `drop_ok` triggers, are not
/// named.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DroppedReaction {
    /// The entity whose action fired the reaction.
    pub source_entity: String,
    /// That action.
    pub source_action: String,
    /// The trigger's name.
    pub trigger_name: String,
    /// The entity the reaction was sent to.
    pub target_entity: String,
    /// The action it asked for.
    pub target_action: String,
    /// The target's status when the reaction landed.
    pub target_state: String,
}

impl fmt::Display for DroppedReaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} fired '{}' -> {}.{}, but {} was in '{}'",
            self.source_entity,
            self.source_action,
            self.trigger_name,
            self.target_entity,
            self.target_action,
            self.target_entity,
            self.target_state
        )
    }
}

/// What a drop check found.
pub(crate) struct Drops {
    /// Each distinct dropped reaction, in order.
    pub(crate) drops: Vec<DroppedReaction>,
    /// Distinct joint states walked.
    pub(crate) states: usize,
    /// Whether every reachable joint state was walked.
    pub(crate) complete: bool,
}

impl JointModel {
    /// The joint model of every type in `scope`, whole (nothing sliced), one
    /// record of each that every reference finds, the dispatches a move
    /// sends landing before the next move from outside. Anyone sends
    /// anything, as ADR-0150's check assumed.
    pub(in crate::composite::joint) fn whole(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: BTreeSet<String>,
    ) -> Self {
        let (models, reactions) = tracked(by_name, &scope, BTreeMap::new(), Records::One);
        // The id fields references to types in scope use: in guards, in
        // reactions' guards, and in field-resolved reactions.
        let guards = models
            .iter()
            .flat_map(|(entity, model)| model.transitions.iter().map(move |t| (entity, &t.guard)))
            .chain(
                reactions
                    .iter()
                    .filter_map(|r| r.guard.as_ref().map(|guard| (&r.from, guard))),
            );
        let mut refs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (entity, guard) in guards {
            for (related, id_field) in guard.related_types() {
                if scope.contains(&related) {
                    refs.entry(entity.clone()).or_default().insert(id_field);
                }
            }
        }
        for reaction in &reactions {
            if let Some(TargetResolver::Field { id_field }) = &reaction.resolve {
                refs.entry(reaction.from.clone())
                    .or_default()
                    .insert(id_field.clone());
            }
        }
        let mut model = Self::compose(
            by_name,
            scope,
            (models, reactions),
            &BTreeMap::new(),
            Sending::anyone(),
            false,
        );
        model.records = Records::One;
        model.refs = refs;
        model.delivery = Delivery::BeforeNextRequest;
        model
    }

    /// Walk the settled joint states (nothing in flight) breadth first, at
    /// most `states` of them, naming each distinct reaction a target refuses
    /// when it lands. A move from outside settles through every order its
    /// dispatches can land in before the next one.
    pub(in crate::composite::joint) fn drops(&self, states: usize) -> Drops {
        // `states` bounds distinct settled states, not the work needed to
        // reach them. Give the default 250,000-state walk 10M expansion
        // steps, the install check's work cap; scale smaller requests down.
        // Parameter products, reaction branches and settling still spend
        // this finite allowance and exhaustion still refuses proof.
        let work = states.saturating_mul(40).min(10_000_000);
        self.expansion.reset(work, None);
        let mut seen: HashSet<JointState> = HashSet::new();
        let mut queue = VecDeque::new();
        for init in self.inits() {
            if !self.expansion.take() {
                break;
            }
            if seen.insert(init.clone()) {
                queue.push_back(init);
            }
        }
        let mut drops = BTreeSet::new();
        let mut complete = true;
        while let Some(state) = queue.pop_front() {
            if seen.len() >= states || self.expansion.exhausted.get() {
                complete = false;
                break;
            }
            for mv in self.moves(&state) {
                if self.expansion.exhausted.get() {
                    break;
                }
                for next in self.next(&state, &mv).0 {
                    for settled in self.settle(next.state, &mut drops) {
                        if seen.insert(settled.clone()) {
                            queue.push_back(settled);
                        }
                    }
                }
            }
        }
        Drops {
            drops: drops.into_iter().collect(),
            states: seen.len(),
            complete: complete && !self.expansion.exhausted.get(),
        }
    }

    /// The settled states `state` reaches as its dispatches land, in every
    /// order, adding to `drops` each reaction refused on the way.
    fn settle(&self, state: JointState, drops: &mut BTreeSet<DroppedReaction>) -> Vec<JointState> {
        let mut settled = Vec::new();
        let mut seen: HashSet<JointState> = HashSet::new();
        let mut stack = vec![state];
        while let Some(state) = stack.pop() {
            if !self.expansion.take() {
                break;
            }
            if state.inflight.is_empty() {
                settled.push(state);
                continue;
            }
            for mv in self.moves(&state) {
                if self.expansion.exhausted.get() {
                    break;
                }
                if let Some(drop) = self.dropped(&state, &mv) {
                    drops.insert(drop);
                }
                for next in self.next(&state, &mv).0 {
                    if seen.insert(next.state.clone()) {
                        stack.push(next.state);
                    }
                }
            }
        }
        settled
    }

    /// The reaction `mv` drops: a dispatch a reaction sent that its target
    /// refuses, on the dispatch path or in its actor.
    fn dropped(&self, state: &JointState, mv: &Move) -> Option<DroppedReaction> {
        let (Move::Read(dispatch) | Move::Arrive(dispatch)) = mv else {
            return None;
        };
        let reaction = &self.reactions[dispatch.via?];
        if reaction.may_drop {
            return None;
        }
        let refused = if dispatch.read {
            let mut rest = state.clone();
            remove(&mut rest, dispatch);
            matches!(self.arrive(&rest, dispatch), Arrival::Refused)
        } else {
            self.read_allows(dispatch, state) == Truth::False
        };
        (refused && !self.expansion.exhausted.get()).then(|| DroppedReaction {
            source_entity: reaction.from.clone(),
            source_action: reaction.source_action.clone(),
            trigger_name: reaction.trigger.clone(),
            target_entity: dispatch.to.clone(),
            target_action: dispatch.action.clone(),
            target_state: state.records[&dispatch.to].status.clone(),
        })
    }
}

/// Walk the joint model of the types in `scope`, whole, one record of each,
/// at most `states` joint states, and name every reaction a target refuses
/// when it lands (ADR-0150). The dispatches a move sends land, in any order,
/// before the next move from outside: the check claims no more than that.
pub(crate) fn dropped_reactions(
    automatons: &[&Automaton],
    scope: BTreeSet<String>,
    states: usize,
) -> Drops {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    JointModel::whole(&by_name, scope).drops(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::joint::Dispatch;

    #[test]
    fn settling_stops_inside_its_expansion_budget() {
        let a = temper_spec::automaton::parse_automaton(
            r#"
[automaton]
name = "T"
states = ["Ready"]
initial = "Ready"
[[action]]
name = "Stay"
kind = "input"
from = ["Ready"]
to = "Ready"
"#,
        )
        .unwrap();
        let model = JointModel::whole(&BTreeMap::from([("T", &a)]), BTreeSet::from(["T".into()]));
        let mut state = model.inits().next().unwrap();
        state.inflight = (0..4)
            .map(|depth| Dispatch {
                to: "T".into(),
                action: "Stay".into(),
                target_state: None,
                params: BTreeMap::new(),
                read: true,
                depth,
                via: None,
                by: super::super::super::senders::OUTSIDE,
                inline: false,
            })
            .collect();
        model.expansion.reset(3, None);
        let settled = model.settle(state, &mut BTreeSet::new());
        assert!(settled.is_empty());
        assert!(model.expansion.exhausted.get());
    }
}
