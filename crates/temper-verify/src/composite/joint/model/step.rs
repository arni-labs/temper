//! The moves of the joint model and the states they lead to: actions taken
//! or sent, dispatches read on the dispatch path, and dispatches their
//! records take or refuse.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Arg, Literal, Truth, eval};

use super::super::env::{Blind, Found, ID, resolve};
use super::super::{Dispatch, JointState};
use super::{Delivery, JointModel, MAX_IN_FLIGHT, MAX_REACTION_DEPTH, Reaction, canonical};
use crate::model::semantics::{step, successors_budgeted};
use crate::model::{
    ModelString, ParamValue, ResolvedTransition, StringParam, TemperModelAction, TemperModelState,
};

/// One move of the walk.
#[derive(Debug, Clone)]
pub(in crate::composite::joint) enum Move {
    /// An action whose guard and constraints read only its own record: its
    /// actor checks them as it writes, so sending and taking are one step.
    Take(String, TemperModelAction),
    /// An action whose guard reads related records, sent from outside: the
    /// dispatch path read them and they allowed it; its actor takes it
    /// later.
    Send(String, TemperModelAction),
    /// The dispatch path reads the related records a reaction needs.
    Read(Dispatch),
    /// The target's actor takes a dispatch, or refuses it.
    Arrive(Dispatch),
}

/// A state a move leads to.
pub(in crate::composite::joint) struct Next {
    pub(in crate::composite::joint) state: JointState,
    /// The new names of the values the old state held.
    pub(in crate::composite::joint) renamed: BTreeMap<ModelString, ModelString>,
    /// The move, named as in the new state.
    pub(in crate::composite::joint) label: String,
}

impl JointModel {
    /// Every move from `state`: each record's enabled actions, with string
    /// parameters chosen among the values held anywhere in the joint state
    /// (when its delivery lets a move from outside come now), and each
    /// dispatch in flight.
    pub(in crate::composite::joint) fn moves(&self, state: &JointState) -> Vec<Move> {
        let values = self.values(state);
        let mut out = Vec::new();
        let asked = self.delivery == Delivery::Any || state.inflight.is_empty();
        for (entity, model) in self.models.iter().filter(|_| asked) {
            let local = &state.records[entity];
            for t in &model.transitions {
                if !self.expansion.take() {
                    return out;
                }
                if !self.enabled(entity, t, state, Blind::Nothing) {
                    continue;
                }
                let reads = self.reads_related(t);
                for (params, _) in successors_budgeted(model, t, local, &values, &self.expansion) {
                    let action = TemperModelAction {
                        name: t.name.clone(),
                        target_state: t.to_state.clone(),
                        params,
                    };
                    out.push(if reads {
                        Move::Send(entity.clone(), action)
                    } else {
                        Move::Take(entity.clone(), action)
                    });
                }
            }
        }
        let mut seen = BTreeSet::new();
        for dispatch in &state.inflight {
            if seen.insert(dispatch) {
                out.push(if dispatch.read {
                    Move::Arrive(dispatch.clone())
                } else {
                    Move::Read(dispatch.clone())
                });
            }
        }
        out
    }

    /// The states `mv` leads to from `state` (more than one when a
    /// reaction's guard the model cannot decide may go either way, the one
    /// where it does not fire first), and whether a state was held back
    /// because it would put more than [`MAX_IN_FLIGHT`] dispatches in flight.
    pub(in crate::composite::joint) fn next(
        &self,
        state: &JointState,
        mv: &Move,
    ) -> (Vec<Next>, bool) {
        if !self.expansion.take() {
            return (Vec::new(), false);
        }
        let mut outcomes: Vec<(JointState, String)> = Vec::new();
        match mv {
            Move::Take(entity, action) => {
                let model = &self.models[entity];
                let Some(moved) = step(
                    model,
                    &action.name,
                    action.target_state.as_deref(),
                    &state.records[entity],
                    &action.params,
                ) else {
                    return (Vec::new(), false);
                };
                let mut next = state.clone();
                next.records.insert(entity.clone(), moved);
                let text = format!("{entity}.{action}");
                for outcome in self.react(next, entity, &action.name, 0) {
                    outcomes.push((outcome, text.clone()));
                }
            }
            Move::Send(entity, action) => {
                let mut next = state.clone();
                add(
                    &mut next,
                    Dispatch {
                        to: entity.clone(),
                        action: action.name.clone(),
                        target_state: action.target_state.clone(),
                        params: action.params.clone(),
                        read: true,
                        depth: 0,
                        via: None,
                    },
                );
                outcomes.push((next, format!("{entity}.{action} sent")));
            }
            Move::Read(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                if self.read_allows(dispatch, state) {
                    let mut read = dispatch.clone();
                    read.read = true;
                    add(&mut next, read);
                    outcomes.push((next, format!("{dispatch}: related records read")));
                } else {
                    outcomes.push((next, format!("{dispatch} refused when read")));
                }
            }
            Move::Arrive(dispatch) => {
                let mut next = state.clone();
                remove(&mut next, dispatch);
                match self.arrive(&next, dispatch) {
                    Some((moved, name)) => {
                        let text = format!("{dispatch} arrives -> {}", moved.status);
                        next.records.insert(dispatch.to.clone(), moved);
                        for outcome in self.react(next, &dispatch.to, &name, dispatch.depth) {
                            outcomes.push((outcome, text.clone()));
                        }
                    }
                    None => outcomes.push((next, format!("{dispatch} refused by its record"))),
                }
            }
        }
        let mut held_back = false;
        let nexts = outcomes
            .into_iter()
            .filter(|(outcome, _)| {
                let fits = self.delivery == Delivery::BeforeNextRequest
                    || outcome.inflight.len() <= MAX_IN_FLIGHT;
                held_back |= !fits;
                fits
            })
            .map(|(mut outcome, text)| {
                let renamed = canonical(&mut outcome);
                Next {
                    state: outcome,
                    label: rename_label(&text, mv, &renamed),
                    renamed,
                }
            })
            .collect();
        (nexts, held_back)
    }

    /// Whether the dispatch path lets `dispatch` through: the related
    /// records its guard reads allow it. Its own record is read later, by
    /// its actor.
    pub(super) fn read_allows(&self, dispatch: &Dispatch, state: &JointState) -> bool {
        self.transitions_for(dispatch)
            .any(|t| eval(&t.guard, &self.env_blind(&dispatch.to, state, Blind::Own)).may_hold())
    }

    fn transitions_for<'a>(
        &'a self,
        dispatch: &'a Dispatch,
    ) -> impl Iterator<Item = &'a ResolvedTransition> + 'a {
        self.models[&dispatch.to]
            .transitions
            .iter()
            .filter(move |t| {
                t.name == dispatch.action
                    && dispatch
                        .target_state
                        .as_ref()
                        .is_none_or(|target| t.to_state.as_ref() == Some(target))
            })
    }

    /// The target's record after its actor takes `dispatch`, and the action
    /// it took; `None` when it refuses. The actor checks its own state; the
    /// related records were read on the dispatch path.
    pub(super) fn arrive(
        &self,
        state: &JointState,
        dispatch: &Dispatch,
    ) -> Option<(TemperModelState, String)> {
        let model = &self.models[&dispatch.to];
        let target = &state.records[&dispatch.to];
        let t = self
            .transitions_for(dispatch)
            .find(|t| self.enabled(&dispatch.to, t, state, Blind::Related))?;
        // Counts and booleans an effect reads and the dispatch does not
        // carry: the first choice, as the composite check takes. A string
        // parameter it does not carry leaves the field as it is.
        let values = self.values(state);
        let mut params: BTreeMap<String, ParamValue> =
            successors_budgeted(model, t, target, &values, &self.expansion)
                .into_iter()
                .next()
                .map(|(params, _)| params)
                .unwrap_or_default();
        for param in &t.string_params {
            params.remove(&param.name);
        }
        params.extend(dispatch.params.clone());
        if !constraints_hold(&t.string_params, target, &params) {
            return None;
        }
        let moved = step(model, &t.name, t.to_state.as_deref(), target, &params)?;
        Some((moved, t.name.clone()))
    }

    /// Send the reactions of `source.action` between types in scope, in
    /// declaration order, as the runtime does after the source commits:
    /// each is a dispatch carrying its `args` as the source holds them now.
    /// A reaction fires when its guard holds on the source; one whose guard
    /// the model cannot decide (related records are read later) may go
    /// either way, so both outcomes are kept. A reaction whose target
    /// reference finds no record goes nowhere.
    fn react(&self, state: JointState, source: &str, action: &str, depth: u32) -> Vec<JointState> {
        let mut outcomes = vec![state];
        if depth >= MAX_REACTION_DEPTH {
            return outcomes;
        }
        for (index, reaction) in self
            .reactions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.from == source && r.source_action == action)
        {
            let mut after = Vec::new();
            for state in outcomes {
                if !self.expansion.take() {
                    return Vec::new();
                }
                let fires = match &reaction.guard {
                    Some(guard) => eval(guard, &self.env_blind(source, &state, Blind::Related)),
                    None => Truth::True,
                };
                if fires != Truth::True {
                    after.push(state.clone());
                }
                if fires == Truth::False {
                    continue;
                }
                let reaches = match &reaction.resolve {
                    Some(TargetResolver::SameId) => {
                        resolve(&self.models, self.records, &state, source, ID, &reaction.to)
                    }
                    Some(TargetResolver::Field { id_field }) => resolve(
                        &self.models,
                        self.records,
                        &state,
                        source,
                        id_field,
                        &reaction.to,
                    ),
                    _ => Found::Unknown,
                };
                if reaches == Found::Missing {
                    after.push(state);
                    continue;
                }
                let dispatch = Dispatch {
                    to: reaction.to.clone(),
                    action: reaction.target_action.clone(),
                    target_state: None,
                    params: self.args(&state, source, reaction),
                    read: !self.action_reads_related(&reaction.to, &reaction.target_action),
                    depth: depth + 1,
                    via: Some(index),
                };
                let mut next = state;
                add(&mut next, dispatch);
                after.push(next);
            }
            outcomes = after;
        }
        outcomes
    }

    /// The tracked string parameters `reaction` passes, read from `source`
    /// as it holds them.
    fn args(
        &self,
        state: &JointState,
        source: &str,
        reaction: &Reaction,
    ) -> BTreeMap<String, ParamValue> {
        let source_state = &state.records[source];
        let tracked = &self.models[&reaction.to].tracked_strings;
        reaction
            .args
            .iter()
            .filter(|(param, _)| tracked.contains(*param))
            .filter_map(|(param, arg)| {
                let value = match arg {
                    Arg::Var(field) => source_state.strings.get(field).cloned(),
                    Arg::Lit(Literal::Str(text)) => Some(text.clone().into()),
                    Arg::Lit(Literal::Null) => Some("".into()),
                    _ => None,
                }?;
                Some((param.clone(), ParamValue::Str(value)))
            })
            .collect()
    }
}

fn add(state: &mut JointState, dispatch: Dispatch) {
    let at = state.inflight.partition_point(|d| d < &dispatch);
    state.inflight.insert(at, dispatch);
}

pub(super) fn remove(state: &mut JointState, dispatch: &Dispatch) {
    if let Some(at) = state.inflight.iter().position(|d| d == dispatch) {
        state.inflight.remove(at);
    }
}

/// Render the move with renamed symbol parameters, preserving concrete literals.
fn rename_label(text: &str, mv: &Move, renamed: &BTreeMap<ModelString, ModelString>) -> String {
    let rename_params = |params: &mut BTreeMap<String, ParamValue>| {
        for value in params.values_mut() {
            if let ParamValue::Str(s) = value
                && let Some(new) = renamed.get(s)
            {
                *s = new.clone();
            }
        }
    };
    let (before, after) = match mv {
        Move::Take(entity, action) | Move::Send(entity, action) => {
            let mut new = action.clone();
            rename_params(&mut new.params);
            (format!("{entity}.{action}"), format!("{entity}.{new}"))
        }
        Move::Read(dispatch) | Move::Arrive(dispatch) => {
            let mut new = dispatch.clone();
            rename_params(&mut new.params);
            (dispatch.to_string(), new.to_string())
        }
    };
    format!(
        "{after}{}",
        text.strip_prefix(&before)
            .expect("label starts with its move")
    )
}

/// Whether the string parameters `params` passes satisfy the action's
/// constraints on its own record, which its actor checks as it writes.
fn constraints_hold(
    string_params: &[StringParam],
    target: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> bool {
    string_params.iter().all(|param| {
        // A string parameter nobody passed is absent, which only a
        // non-empty constraint refuses.
        let Some(ParamValue::Str(value)) = params.get(&param.name) else {
            return !param.nonempty;
        };
        let tracked = |var: &String| target.strings.get(var);
        !(param.nonempty && value.is_empty())
            && param
                .equals
                .as_ref()
                .and_then(tracked)
                .is_none_or(|v| v == value)
            && param
                .not_equals
                .iter()
                .filter_map(tracked)
                .all(|v| v != value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_a_move_preserves_a_literal_that_spells_a_symbol() {
        let action = TemperModelAction {
            name: "Set".into(),
            target_state: None,
            params: BTreeMap::from([
                ("a".into(), ParamValue::Str(ModelString::from("#2"))),
                ("b".into(), ParamValue::Str(ModelString::Symbol(2))),
            ]),
        };
        let text = format!("T.{action} sent");
        let label = rename_label(
            &text,
            &Move::Send("T".into(), action),
            &BTreeMap::from([(ModelString::Symbol(2), ModelString::Symbol(1))]),
        );
        assert_eq!(label, "T.Set(a='#2', b='#1') sent");
    }
}
