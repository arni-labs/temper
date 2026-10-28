//! String choices and canonical equality symbols.

use super::expansion::Expansion;
use super::types::{
    ModelString, ParamValue, ResolvedTransition, StringParam, TemperModel, TemperModelState,
};
use std::collections::{BTreeMap, BTreeSet};

/// The tracked string values held by `states`, `''` left out.
pub fn string_values<'a>(
    states: impl IntoIterator<Item = &'a TemperModelState>,
) -> BTreeSet<ModelString> {
    states
        .into_iter()
        .flat_map(|state| state.strings.values())
        .filter(|value| !value.is_empty())
        .cloned()
        .collect()
}

/// A value equal to none of `values`: `#n` past the largest `#n` in use.
pub(crate) fn fresh_string(values: &BTreeSet<ModelString>) -> ModelString {
    let largest = values
        .iter()
        .filter_map(|value| match value {
            ModelString::Symbol(n) => Some(*n),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    ModelString::Symbol(largest + 1)
}

/// The values one string parameter may take in `state`.
fn string_candidates(
    model: &TemperModel,
    param: &StringParam,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
) -> Vec<ModelString> {
    let tracked = |var: &str| state.strings.get(var).cloned();
    let mut candidates: BTreeSet<ModelString> =
        match param.equals.iter().find_map(|var| tracked(var)) {
            Some(value) => BTreeSet::from([value]),
            None => {
                let mut all: BTreeSet<ModelString> = values.clone();
                all.insert("".into());
                all.insert(fresh_string(values));
                if let Some(literals) = model.string_literals.get(&param.name) {
                    all.extend(literals.iter().cloned().map(ModelString::from));
                }
                all
            }
        };
    // Every equality must hold, including when two fields disagree.
    for var in &param.equals {
        if let Some(value) = tracked(var) {
            candidates.retain(|candidate| candidate == &value);
        }
    }
    if param.nonempty {
        candidates.retain(|candidate| !candidate.is_blank());
    }
    for var in &param.not_equals {
        if let Some(value) = tracked(var) {
            candidates.remove(&value);
        }
    }
    candidates.into_iter().collect()
}

/// Every choice of values for `t`'s string parameters (one empty choice when
/// it has none; none when a constraint leaves a parameter no value).
pub(super) fn string_assignments(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
) -> Vec<BTreeMap<String, ModelString>> {
    let mut assignments = vec![BTreeMap::new()];
    for param in &t.string_params {
        let mut more = Vec::new();
        for assignment in assignments {
            if !budget.take() {
                return Vec::new();
            }
            let mut scope = values.clone();
            scope.extend(assignment.values().cloned());
            let mut candidates = string_candidates(model, param, state, &scope);
            // A parameter that writes nothing leads to the same state with any
            // value its constraints allow: one is enough.
            if !param.writes {
                candidates.truncate(1);
            }
            for value in candidates {
                if !budget.take() {
                    return Vec::new();
                }
                let mut next = assignment.clone();
                next.insert(param.name.clone(), value);
                more.push(next);
            }
        }
        assignments = more;
    }
    assignments
}

/// Write each string parameter that writes a tracked variable into it, as
/// the runtime writes every parameter into the field of its name.
pub fn apply_string_params(
    string_params: &[StringParam],
    state: &mut TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) {
    for param in string_params.iter().filter(|p| p.writes) {
        if let Some(ParamValue::Str(value)) = params.get(&param.name) {
            state.strings.insert(param.name.clone(), value.clone());
        }
    }
}

/// Renumber the `#n` values of `maps`, in order, by first use: two states
/// that differ only in which new values they made up are one state. Equality
/// between values is all an invariant can see of them, so nothing is lost.
/// Returns each value's new name.
pub fn canonical_strings(
    maps: &mut [&mut BTreeMap<String, ModelString>],
) -> BTreeMap<ModelString, ModelString> {
    let mut renamed: BTreeMap<ModelString, ModelString> = BTreeMap::new();
    for map in maps.iter_mut() {
        for value in map.values_mut() {
            if !matches!(value, ModelString::Symbol(_)) {
                continue;
            }
            let next = ModelString::Symbol(renamed.len() + 1);
            let name = renamed.entry(value.clone()).or_insert(next).clone();
            *value = name;
        }
    }
    renamed
}
