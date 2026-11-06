//! What the kernel writes into a parameter it binds (`source`) when it makes
//! a move itself: a reaction, a timer, or a module's answer or failure.

use std::collections::BTreeSet;

use super::super::Dispatch;
use super::JointModel;
use super::commit::constraints_hold;
use crate::model::semantics::successors_budgeted;
use crate::model::{Fields, ModelString, ParamValue, ResolvedTransition, Sym, TemperModelState};
use temper_authz::May;

impl JointModel {
    /// The values `t`'s tracked bound parameters may take when `dispatch`
    /// arrives at `target`: one set per choice, one empty set when it binds
    /// none the model tracks, none when the walk cannot make one within its
    /// bounds. `Err` when the kernel refuses the move: the dispatch carries
    /// a bound parameter, which no one may supply, or it is a reaction sent
    /// as its own principal, a service, which names no caller. Otherwise the
    /// kernel binds them from whoever set the chain off, and refuses the move
    /// when that is no caller, so they take any value a sender could hold:
    /// never empty for the caller's id, anything for an agent's label.
    pub(super) fn caller_ids(
        &self,
        dispatch: &Dispatch,
        t: &ResolvedTransition,
        target: &TemperModelState,
        values: &BTreeSet<ModelString>,
    ) -> Result<Vec<Fields<ParamValue>>, ()> {
        if t.bound.is_empty() {
            return Ok(vec![Fields::new()]);
        }
        let reaction = dispatch.via.map(|via| &self.reactions[via]);
        let as_service = reaction.is_some_and(|reaction| reaction.principal.is_some());
        let sends = |name: &Sym| {
            dispatch.params.contains_key(name)
                || reaction.is_some_and(|reaction| reaction.args.contains_key(name.as_str()))
        };
        let named = self.senders.names_a_caller(dispatch.by);
        if as_service || named == May::No || t.bound.iter().any(sends) {
            return Err(());
        }
        let tracked: Vec<_> = t
            .string_params
            .iter()
            .filter(|param| param.source.is_some())
            .map(|param| param.name)
            .collect();
        if tracked.is_empty() {
            return Ok(vec![Fields::new()]);
        }
        let model = &self.models[dispatch.to.as_str()];
        let choices: BTreeSet<Fields<ParamValue>> =
            successors_budgeted(model, t, target, values, &self.expansion)
                .into_iter()
                .map(|(params, _)| {
                    tracked
                        .iter()
                        .filter_map(|name| params.get(name).map(|value| (*name, *value)))
                        .collect()
                })
                .collect();
        Ok(choices.into_iter().collect())
    }

    /// Whether the kernel surely binds what `entity`'s `action` binds when
    /// `by` makes it: it binds nothing, or `by` surely names a caller. The
    /// kernel refuses the move when the chain names none.
    pub(super) fn binds_surely(&self, entity: &str, action: &str, by: u32) -> bool {
        self.senders.names_a_caller(by) == May::Yes
            || self.models[entity]
                .transitions
                .iter()
                .filter(|t| t.name == action)
                .all(|t| t.bound.is_empty())
    }
}

/// `params` with each choice of the kernel's bound values, where `t`'s
/// constraints hold.
pub(super) fn with_callers(
    params: &Fields<ParamValue>,
    callers: Vec<Fields<ParamValue>>,
    t: &ResolvedTransition,
    target: &TemperModelState,
) -> Vec<Fields<ParamValue>> {
    callers
        .into_iter()
        .map(|caller| {
            let mut params = params.clone();
            params.extend(caller.iter().map(|(name, value)| (*name, *value)));
            params
        })
        .filter(|params| constraints_hold(&t.string_params, target, params))
        .collect()
}
