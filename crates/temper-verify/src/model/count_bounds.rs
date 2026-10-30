//! Numeric parameter domains follow dependencies through ordered effects.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::Arg;

use super::semantics::counter_bound;
use super::types::{ModelEffect, ResolvedTransition, TemperModel};

/// The largest value needed by effects that read this count parameter,
/// directly or through a counter written earlier in the transition.
/// Unrelated counters do not expand its domain, and an overwrite ends a
/// dependency. List indexes include an out-of-range no-op even after appends.
pub(super) fn count_bound(model: &TemperModel, t: &ResolvedTransition, name: &str) -> usize {
    let mut dependent = BTreeSet::new();
    let mut list_lengths = BTreeMap::new();
    let mut bound = None;
    for effect in &t.effects {
        match effect {
            ModelEffect::SetCounter { var, value }
            | ModelEffect::AddCounter { var, value }
            | ModelEffect::SubCounter { var, value } => {
                let reads = reads_parameter(value, name, &dependent);
                let depends = reads
                    || (!matches!(effect, ModelEffect::SetCounter { .. })
                        && dependent.contains(var));
                if depends {
                    bound = Some(bound.unwrap_or(0).max(counter_bound(model, var)));
                    dependent.insert(var.clone());
                } else {
                    dependent.remove(var);
                }
            }
            ModelEffect::ListAppend { var, .. } => {
                let length = list_lengths
                    .entry(var)
                    .or_insert_with(|| list_bound(model, var));
                *length = length.saturating_add(1);
            }
            ModelEffect::ListRemoveAt { var, index }
                if reads_parameter(index, name, &dependent) =>
            {
                let length = list_lengths
                    .get(var)
                    .copied()
                    .unwrap_or_else(|| list_bound(model, var));
                bound = Some(bound.unwrap_or(0).max(length));
            }
            _ => {}
        }
    }
    bound.unwrap_or(model.default_max_counter)
}

fn reads_parameter(arg: &Arg, name: &str, dependent: &BTreeSet<String>) -> bool {
    match arg {
        Arg::Param(p) => p == name,
        Arg::Var(var) => dependent.contains(var),
        Arg::Lit(_) => false,
    }
}

fn list_bound(model: &TemperModel, var: &str) -> usize {
    // Removals can be no-ops, so retain a conservative length through them.
    model
        .default_max_counter
        .max(model.initial_lists.get(var).map_or(0, Vec::len))
}
