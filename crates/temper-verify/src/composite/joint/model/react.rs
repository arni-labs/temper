//! What a record's commit sets off: its reactions, as dispatches, and the
//! module calls its triggers start.

use std::collections::BTreeMap;

use temper_spec::automaton::TargetResolver;
use temper_spec::predicate::{Arg, Literal, Truth, eval};

use super::super::env::{Blind, Found, ID, resolve};
use super::super::senders::RUNTIME;
use super::super::{Call, Dispatch, JointState};
use super::commit::add;
use super::{JointModel, MAX_REACTION_DEPTH, Reaction};
use crate::model::ParamValue;

impl JointModel {
    /// What follows `source.action` committing, sent by `by`: its
    /// reactions, then the module calls its triggers start (when the model
    /// makes the kernel's moves). The calls run in the background and answer
    /// as the runtime, or, when `inline` (the sender waits for them), may
    /// instead run inline and answer as `by`. Each outcome says whether the
    /// model knows it happens.
    pub(super) fn after_commit(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        depth: u32,
        by: u32,
        inline: bool,
    ) -> Vec<(JointState, bool)> {
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
        let mut started = Vec::new();
        for (outcome, sure) in outcomes {
            for &answer_as in &answering {
                let mut outcome = outcome.clone();
                for spec in specs {
                    let call = Call {
                        to: source.to_string(),
                        module: spec.module.clone(),
                        on_success: spec.on_success.clone(),
                        on_failure: spec.on_failure.clone(),
                        by: answer_as,
                    };
                    let at = outcome.calls.partition_point(|c| c < &call);
                    outcome.calls.insert(at, call);
                }
                started.push((outcome, sure));
            }
        }
        started
    }

    /// Send the reactions of `source.action` between types in scope, in
    /// declaration order, as the runtime does after the source commits:
    /// each is a dispatch carrying its `args` as the source holds them now.
    /// A reaction fires when its guard holds on the source; one whose guard
    /// the model cannot decide (related records are read later) may go
    /// either way, so both outcomes are kept, neither of them certain. A
    /// reaction whose target reference finds no record goes nowhere.
    fn react(
        &self,
        state: JointState,
        source: &str,
        action: &str,
        depth: u32,
        by: u32,
    ) -> Vec<(JointState, bool)> {
        let mut outcomes = vec![(state, true)];
        if depth >= MAX_REACTION_DEPTH {
            return outcomes;
        }
        for reaction in self
            .reactions
            .iter()
            .filter(|r| r.from == source && r.source_action == action)
        {
            let mut after = Vec::new();
            for (state, certain) in outcomes {
                let fires = match &reaction.guard {
                    Some(guard) => eval(guard, &self.env_blind(source, &state, Blind::Related)),
                    None => Truth::True,
                };
                let decided = fires != Truth::Unknown;
                if fires != Truth::True {
                    after.push((state.clone(), certain && decided));
                }
                if fires == Truth::False {
                    continue;
                }
                let reaches = match &reaction.resolve {
                    Some(TargetResolver::SameId) => {
                        resolve(&self.models, &state, source, ID, &reaction.to)
                    }
                    Some(TargetResolver::Field { id_field }) => {
                        resolve(&self.models, &state, source, id_field, &reaction.to)
                    }
                    _ => Found::Unknown,
                };
                if reaches == Found::Missing {
                    after.push((state, certain && decided));
                    continue;
                }
                let dispatch = Dispatch {
                    to: reaction.to.clone(),
                    action: reaction.target_action.clone(),
                    target_state: None,
                    params: self.args(&state, source, reaction),
                    read: !self.action_reads_related(&reaction.to, &reaction.target_action),
                    depth: depth + 1,
                    by: self.senders.reaction(reaction.principal.as_ref(), by),
                    inline: false,
                };
                let mut next = state;
                add(&mut next, dispatch);
                after.push((next, certain && decided && reaches == Found::Yes));
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
                    Arg::Lit(Literal::Str(text)) => Some(text.clone()),
                    Arg::Lit(Literal::Null) => Some(String::new()),
                    _ => None,
                }?;
                Some((param.clone(), ParamValue::Str(value)))
            })
            .collect()
    }
}
