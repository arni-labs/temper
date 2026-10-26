//! What a record's commit sets off: its reactions, as dispatches, and the
//! module calls its triggers start, with the answers a module may give.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Arg, Literal, ParamKind, Truth, eval};

use super::super::env::{Blind, Found, ID, resolve};
use super::super::senders::RUNTIME;
use super::super::{Call, Dispatch, JointState};
use super::commit::add;
use super::step::{Certainty, Move};
use super::{JointModel, MAX_REACTION_DEPTH, Reaction};
use crate::model::semantics::successors_budgeted;
use crate::model::{ModelString, ParamValue, TemperModelAction};

impl JointModel {
    /// What follows `source.action` committing, sent by `by`: its
    /// reactions, then the module calls its triggers start (when the model
    /// makes the kernel's moves). The calls run in the background and answer
    /// as the runtime, or, when `inline` (the sender waits for them), may
    /// instead run inline and answer as `by`. Each outcome says what the
    /// model knows of it.
    pub(super) fn after_commit(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        depth: u32,
        by: u32,
        inline: bool,
    ) -> Vec<(JointState, Certainty)> {
        let outcomes = self.react(state, source, action, depth, by);
        let key = (source.to_string(), action.to_string());
        let Some(specs) = self.kernel.calls_on.get(&key).filter(|_| self.kernel_moves) else {
            return outcomes;
        };
        let answering = if inline && by != RUNTIME && self.sender_matters.contains(source) {
            vec![RUNTIME, by]
        } else {
            vec![RUNTIME]
        };
        let internal = self.kernel.internal.get(source);
        let asked: Vec<BTreeSet<String>> = specs
            .iter()
            .map(|spec| {
                self.unchecked
                    .asked(source, |action| match &spec.on_success {
                        Some(name) => name == action,
                        None => internal.is_some_and(|names| names.contains(action)),
                    })
            })
            .collect();
        let mut started = Vec::new();
        for (outcome, known) in outcomes {
            for &answer_as in &answering {
                if !self.expansion.take() {
                    return Vec::new();
                }
                let mut outcome = outcome.clone();
                for (spec, asked) in specs.iter().zip(&asked) {
                    let strings = &outcome.records[source].strings;
                    let call = Call {
                        to: source.to_string(),
                        module: spec.module.clone(),
                        on_success: spec.on_success.clone(),
                        on_failure: spec.on_failure.clone(),
                        by: answer_as,
                        asked: asked
                            .iter()
                            .filter_map(|field| Some((field.clone(), strings.get(field)?.clone())))
                            .collect(),
                    };
                    let at = outcome.calls.partition_point(|c| c < &call);
                    outcome.calls.insert(at, call);
                }
                started.push((outcome, known));
            }
        }
        started
    }

    /// The answers a module may give the call `call`: its trigger's
    /// `on_success`, or else any internal action of its record. The module
    /// itself is not modelled: it may give each of these answers, and gives
    /// each it keeps being able to give at some point.
    pub(super) fn answers(
        &self,
        state: &JointState,
        call: &Call,
        values: &BTreeSet<ModelString>,
        out: &mut Vec<Move>,
    ) {
        let model = &self.models[&call.to];
        let local = &state.records[&call.to];
        for t in &model.transitions {
            let answers = match &call.on_success {
                Some(name) => &t.name == name,
                None => self
                    .kernel
                    .internal
                    .get(&call.to)
                    .is_some_and(|names| names.contains(&t.name)),
            };
            if !answers || !self.enabled(&call.to, t, state, Blind::Nothing) {
                continue;
            }
            let mut seen = BTreeSet::new();
            for (mut params, _) in successors_budgeted(model, t, local, values, &self.expansion) {
                // A parameter tied to a field carries it as the call found it.
                for param in &t.string_params {
                    if let Some(value) = param.equals.as_ref().and_then(|f| call.asked.get(f)) {
                        params.insert(param.name.clone(), ParamValue::Str(value.clone()));
                    }
                }
                if !seen.insert(params.clone()) {
                    continue;
                }
                let action = TemperModelAction {
                    name: t.name.clone(),
                    target_state: t.to_state.clone(),
                    params,
                };
                out.push(Move::Answer(call.clone(), action));
            }
        }
    }

    /// Whether a field `action` ties a parameter to has changed since `call`
    /// started, so its record refuses the module's answer.
    pub(super) fn stale(&self, call: &Call, action: &str, state: &JointState) -> bool {
        let strings = &state.records[&call.to].strings;
        self.unchecked
            .equals
            .get(&(call.to.clone(), action.to_string()))
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|field| call.asked.get(field) != strings.get(field))
            })
    }

    /// Send the reactions of `source.action` between types in scope, in
    /// declaration order, as the runtime does after the source commits:
    /// each is a dispatch carrying its `args` as the source holds them now.
    /// A reaction fires when its guard holds on the source; one whose guard
    /// the model cannot decide (related records are read later) goes one of
    /// the two ways. A reaction whose target reference finds no record goes
    /// nowhere, and one whose reference the model cannot resolve goes one of
    /// those two ways too.
    fn react(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        depth: u32,
        by: u32,
    ) -> Vec<(JointState, Certainty)> {
        let mut outcomes = vec![(state, Certainty::Sure)];
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
            for (state, known) in outcomes {
                if !self.expansion.take() {
                    return Vec::new();
                }
                let fires = match &reaction.guard {
                    Some(guard) => eval(guard, &self.env_blind(source, &state, Blind::Related)),
                    None => Truth::True,
                };
                let known =
                    known.then(Certainty::unless(fires != Truth::Unknown, Certainty::OneOf));
                if fires != Truth::True {
                    after.push((state.clone(), known));
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
                    after.push((state, known));
                    continue;
                }
                let known = known.then(Certainty::unless(reaches == Found::Yes, Certainty::OneOf));
                if reaches == Found::Unknown {
                    after.push((state.clone(), known));
                }
                let dispatch = Dispatch {
                    to: reaction.to.clone(),
                    action: reaction.target_action.clone(),
                    target_state: None,
                    params: self.args(&state, source, reaction),
                    read: !self.action_reads_related(&reaction.to, &reaction.target_action),
                    depth: depth + 1,
                    via: Some(index),
                    by: self.senders.reaction(
                        reaction.principal.as_ref(),
                        by,
                        matches!(reaction.resolve, Some(TargetResolver::SameId)),
                    ),
                    inline: false,
                };
                let mut next = state;
                add(&mut next, dispatch);
                after.push((next, known));
            }
            outcomes = after;
        }
        outcomes
    }

    /// Whether `reaction`'s target action reads its count parameter
    /// `param`: an effect reads it, or a constraint compares it with a
    /// counter.
    fn reads_count(&self, reaction: &Reaction, param: &str) -> bool {
        let (to, action) = (&reaction.to, &reaction.target_action);
        self.unchecked
            .counts(to, action)
            .iter()
            .any(|(counted, _)| counted == param)
            || self.models[to]
                .transitions
                .iter()
                .any(|t| &t.name == action && matches!(t.params.get(param), Some(ParamKind::Count)))
    }

    /// The string parameters `reaction` passes that its target tracks or
    /// requires not empty, the counts kept as sequence numbers, and the
    /// counts its target reads (in an effect, or compared with a counter)
    /// from a counter `source` holds as a value, read from `source` as it
    /// holds them.
    fn args(
        &self,
        state: &JointState,
        source: &str,
        reaction: &Reaction,
    ) -> BTreeMap<String, ParamValue> {
        let source_state = &state.records[source];
        let tracked = &self.models[&reaction.to].tracked_strings;
        let nonempty = self
            .unchecked
            .nonempty(&reaction.to, &reaction.target_action);
        reaction
            .args
            .iter()
            .filter_map(|(param, arg)| {
                if self
                    .order
                    .carries(&reaction.to, &reaction.target_action, param)
                {
                    let Arg::Var(field) = arg else { return None };
                    let count = source_state.counters.get(field).copied()?;
                    return Some((param.clone(), ParamValue::Count(count)));
                }
                if self.reads_count(reaction, param) {
                    // Sent only when the source holds it as a value, not as
                    // a sequence number.
                    let Arg::Var(field) = arg else { return None };
                    if self.order.fields(source).any(|(f, _)| f == field) {
                        return None;
                    }
                    let count = source_state.counters.get(field).copied()?;
                    return Some((param.clone(), ParamValue::Count(count)));
                }
                if !tracked.contains(param) && !nonempty.contains(param) {
                    return None;
                }
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
