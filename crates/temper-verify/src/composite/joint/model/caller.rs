//! What the kernel writes into a parameter bound to the caller's id
//! (`authenticated_subject`) when a dispatch reaches its record.

use std::collections::BTreeSet;

use super::super::Dispatch;
use super::JointModel;
use crate::model::semantics::successors_budgeted;
use crate::model::{Fields, ModelString, ParamValue, ResolvedTransition, TemperModelState};

impl JointModel {
    /// The values `t`'s parameters bound to the caller's id may take when
    /// `dispatch` arrives at `target`: one set per choice, one empty set when
    /// it has none, none when the walk cannot make one within its bounds.
    /// `Err` when the kernel refuses the move: the dispatch carries such a
    /// parameter, which no one may supply, or it is a reaction sent as its
    /// own principal, a service, which names no caller. Otherwise the kernel
    /// writes the id of whoever set the chain off, never empty, and refuses
    /// the move when that is no caller, so any value a sender could hold.
    pub(super) fn caller_ids(
        &self,
        dispatch: &Dispatch,
        t: &ResolvedTransition,
        target: &TemperModelState,
        values: &BTreeSet<ModelString>,
    ) -> Result<Vec<Fields<ParamValue>>, ()> {
        let bound: Vec<_> = t
            .string_params
            .iter()
            .filter(|p| p.caller)
            .map(|p| p.name)
            .collect();
        if bound.is_empty() {
            return Ok(vec![Fields::new()]);
        }
        let as_service = dispatch
            .via
            .is_some_and(|via| self.reactions[via].principal.is_some());
        if as_service || bound.iter().any(|name| dispatch.params.contains_key(name)) {
            return Err(());
        }
        let model = &self.models[dispatch.to.as_str()];
        let choices: BTreeSet<Fields<ParamValue>> =
            successors_budgeted(model, t, target, values, &self.expansion)
                .into_iter()
                .map(|(params, _)| {
                    bound
                        .iter()
                        .filter_map(|name| params.get(name).map(|value| (*name, *value)))
                        .collect()
                })
                .collect();
        Ok(choices.into_iter().collect())
    }
}
