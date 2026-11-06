//! What the kernel writes into a parameter it binds (`source`) when it makes
//! a move itself: a reaction, a timer, or a module's answer or failure.

use std::collections::BTreeSet;

use super::super::Dispatch;
use super::JointModel;
use crate::model::semantics::successors_budgeted;
use crate::model::{Fields, ModelString, ParamValue, ResolvedTransition, Sym, TemperModelState};

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
        if as_service || t.bound.iter().any(sends) {
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

    /// Whether `entity`'s `action` binds no parameter: when it binds one, the
    /// kernel refuses it when the chain that makes it names no caller.
    pub(super) fn binds_nothing(&self, entity: &str, action: &str) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .filter(|t| t.name == action)
            .all(|t| t.bound.is_empty())
    }
}
