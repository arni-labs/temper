//! Shared concrete guard/effect semantics for verification backends.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{
    Arg, Env, Expr, Literal, Operand, ParamKind, Set, Truth, Val, VarKind, eval,
};

use super::types::{
    ModelEffect, ModelString, ParamValue, ResolvedTransition, TemperModel, TemperModelState,
};

pub(crate) use super::expansion::Expansion;
use super::strings::string_assignments;
pub use super::strings::{apply_string_params, canonical_strings, string_values};

/// Parameter assignments explored per step for one transition, at most
/// (numeric/list parameters; joint string products also consume expansion work).
pub const MAX_PARAM_ASSIGNMENTS: usize = 4096;

/// A model state as a predicate environment. Status, counters, booleans,
/// lists and tracked strings are modeled; declared counters, booleans and
/// lists missing from the state read as their zero value. Untracked string
/// and numeric variables, entity fields and related entities are not modeled
/// and read as unknown.
pub struct ModelEnv<'a> {
    /// Declared state variable types.
    pub kinds: &'a BTreeMap<String, VarKind>,
    /// The state being evaluated.
    pub state: &'a TemperModelState,
}

impl Env for ModelEnv<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(&self.state.status)
    }

    fn var(&self, name: &str) -> Val<'_> {
        match self.kinds.get(name) {
            Some(VarKind::Counter) => {
                Val::Num(self.state.counters.get(name).copied().unwrap_or(0) as f64)
            }
            Some(VarKind::Bool) => {
                Val::Bool(self.state.booleans.get(name).copied().unwrap_or(false))
            }
            Some(VarKind::List) => self
                .state
                .lists
                .get(name)
                .map_or(Val::Strs(&[]), |items| Val::Strs(items)),
            Some(VarKind::Str) => self
                .state
                .strings
                .get(name)
                .map_or(Val::Unknown, |value| value.val()),
            Some(VarKind::Num) | None => Val::Unknown,
        }
    }

    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}

/// Three-valued truth of `expr` in `state`.
pub fn truth(expr: &Expr, kinds: &BTreeMap<String, VarKind>, state: &TemperModelState) -> Truth {
    eval(expr, &ModelEnv { kinds, state })
}

/// Whether a guard is **guaranteed** to hold from the local state alone.
///
/// Used by local-enablement checks (`terminal`, `no_deadlock`): a transition
/// gated on something the model cannot see (a related entity's status) is
/// not locally enabled, so a state whose only exit is such a gate is correctly
/// treated as a place the automaton may wait.
pub fn evaluate_guard(
    guard: &Expr,
    kinds: &BTreeMap<String, VarKind>,
    state: &TemperModelState,
) -> bool {
    truth(guard, kinds, state).must_hold()
}

/// Whether a guard **may** hold, for state-space exploration.
///
/// Unknown values are free (nondeterministic) booleans (ADR-0149): the edge
/// is offered when the guard could hold (guard-true branch); the BFS covers the
/// guard-false branch by also exploring states where the edge is not taken.
/// Locally resolvable parts still decide: an unsatisfiable local condition
/// keeps the guard false whatever the unknowns are.
pub fn guard_may_hold(
    guard: &Expr,
    kinds: &BTreeMap<String, VarKind>,
    state: &TemperModelState,
) -> bool {
    truth(guard, kinds, state).may_hold()
}

/// Every state `t` can step to from `state`, with the parameter values
/// chosen for it. The caller has checked `t`'s status and guard.
///
/// A parameter is an unknown value, so each candidate is explored:
/// counts `0..=bound`, both booleans, and for list elements every literal a
/// guard or invariant compares that list against plus one fresh string.
/// Assignments that grow a counter past its bound (or a list past the
/// default bound) are pruned, as in bounded exploration.
pub fn successors(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    successors_among(model, t, state, &string_values(std::iter::once(state)))
}

/// [`successors`], with `values` the tracked strings in scope: the state's
/// own, or every entity's in a joint state. A string parameter may take any
/// of them (a value copied from somewhere), `''`, a literal the spec
/// compares its variable with, or one value new to the scope, as far as the
/// action's constraints allow.
pub fn successors_among(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    successors_budgeted(model, t, state, values, &Expansion::for_model(model))
}

pub(crate) fn successors_budgeted(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    let mut out = Vec::new();
    let strings = string_assignments(model, t, state, values, budget);
    for params in param_assignments(model, t) {
        for chosen in &strings {
            if !budget.take() {
                return out;
            }
            let mut params = params.clone();
            params.extend(
                chosen
                    .iter()
                    .map(|(k, v)| (k.clone(), ParamValue::Str(v.clone()))),
            );
            let mut next = state.clone();
            if let Some(to) = &t.to_state {
                next.status = to.clone();
            }
            apply_effects(&t.effects, &mut next, &t.name, &params);
            apply_string_params(&t.string_params, &mut next, &params);
            canonical_strings(&mut [&mut next.strings]);
            if within_bounds(model, state, &next) {
                out.push((params, next));
            }
        }
    }
    out
}

/// The transition the runtime takes for action `name` in `status`: the first
/// with that name whose source states include `status` (temper-jit
/// `evaluate_ctx`). A later transition with the same name never fires from
/// `status`; when this one's guard fails, the action is refused.
pub fn rule_for<'a>(
    model: &'a TemperModel,
    name: &str,
    status: &str,
) -> Option<&'a ResolvedTransition> {
    model.transitions.iter().find(|t| {
        t.name == name && (t.from_states.is_empty() || t.from_states.iter().any(|s| s == status))
    })
}

/// `state` after transition `t` with `params`, with no renumbering of new
/// strings (the caller renumbers, over one entity or a joint state).
pub fn step(
    t: &ResolvedTransition,
    state: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> TemperModelState {
    let mut next = state.clone();
    if let Some(to) = &t.to_state {
        next.status = to.clone();
    }
    apply_effects(&t.effects, &mut next, &t.name, params);
    apply_string_params(&t.string_params, &mut next, params);
    next
}

/// The transition with more parameter assignments than one step explores
/// ([`MAX_PARAM_ASSIGNMENTS`]), and how many it has. Stepping it would stop
/// at the assertion in `param_assignments`, so a caller asks this first.
pub fn oversized_transition(model: &TemperModel) -> Option<(&str, usize)> {
    let bound = count_bound(model);
    model.transitions.iter().find_map(|t| {
        // Counted, not built: a counter set to a large literal raises the
        // bound, and building its candidates would allocate that many.
        let assignments = t.params.iter().fold(1usize, |n, (name, kind)| {
            let choices = match kind {
                ParamKind::Count => bound.saturating_add(1),
                ParamKind::Bool => 2,
                ParamKind::Str => string_literals(model, t, name).len() + 1,
            };
            n.saturating_mul(choices)
        });
        (assignments > MAX_PARAM_ASSIGNMENTS).then_some((t.name.as_str(), assignments))
    })
}

/// The largest value a count parameter is explored up to.
fn count_bound(model: &TemperModel) -> usize {
    model
        .counter_bounds
        .values()
        .copied()
        .max()
        .unwrap_or(0)
        .max(model.default_max_counter)
}

/// The values explored for one parameter of `t`.
fn param_candidates(
    model: &TemperModel,
    t: &ResolvedTransition,
    name: &str,
    kind: &ParamKind,
    count_bound: usize,
) -> Vec<ParamValue> {
    match kind {
        ParamKind::Count => (0..=count_bound).map(ParamValue::Count).collect(),
        ParamKind::Bool => vec![ParamValue::Bool(false), ParamValue::Bool(true)],
        ParamKind::Str => string_literals(model, t, name)
            .into_iter()
            .map(|s| ParamValue::Str(s.into()))
            .chain(std::iter::once(ParamValue::Fresh))
            .collect(),
    }
}

/// The literals a list `t` appends string parameter `name` to is compared
/// against: the strings worth choosing for it.
fn string_literals(model: &TemperModel, t: &ResolvedTransition, name: &str) -> BTreeSet<String> {
    let mut values: BTreeSet<String> = BTreeSet::new();
    for effect in &t.effects {
        if let ModelEffect::ListAppend {
            var,
            value: Arg::Param(p),
        } = effect
            && p == name
            && let Some(literals) = model.list_literals.get(var)
        {
            values.extend(literals.iter().cloned());
        }
    }
    values
}

/// The cartesian product of each parameter's candidate values.
fn param_assignments(
    model: &TemperModel,
    t: &ResolvedTransition,
) -> Vec<BTreeMap<String, ParamValue>> {
    let count_bound = count_bound(model);
    let mut assignments = vec![BTreeMap::new()];
    for (name, kind) in &t.params {
        let candidates = param_candidates(model, t, name, kind, count_bound);
        assert!(
            assignments.len() * candidates.len() <= MAX_PARAM_ASSIGNMENTS,
            "transition '{}' has more than {MAX_PARAM_ASSIGNMENTS} parameter assignments",
            t.name
        );
        assignments = assignments
            .into_iter()
            .flat_map(|assignment| {
                candidates.iter().map(move |value| {
                    let mut next = assignment.clone();
                    next.insert(name.clone(), value.clone());
                    next
                })
            })
            .collect();
    }
    assignments
}

/// Whether `next` stays within the exploration bounds, or at least grew no
/// counter or list beyond them.
pub(crate) fn within_bounds(
    model: &TemperModel,
    before: &TemperModelState,
    next: &TemperModelState,
) -> bool {
    let counters_ok = next.counters.iter().all(|(var, value)| {
        let bound = model
            .counter_bounds
            .get(var)
            .copied()
            .unwrap_or(model.default_max_counter);
        *value <= bound || *value <= before.counters.get(var).copied().unwrap_or(0)
    });
    let lists_ok = next.lists.iter().all(|(var, items)| {
        items.len() <= model.default_max_counter
            || items.len() <= before.lists.get(var).map_or(0, Vec::len)
    });
    counters_ok && lists_ok
}

/// Apply model effects to the provided state, reading `params.p` from
/// `params`. Mirrors the runtime: `-=` stops at 0 and `remove_at` out of
/// range is a no-op.
///
/// `action_name` is used to generate deterministic fresh list elements.
/// Third interpreter. Production apply is `temper-server` `entity_actor/effects.rs`.
pub fn apply_effects(
    effects: &[ModelEffect],
    state: &mut TemperModelState,
    action_name: &str,
    params: &BTreeMap<String, ParamValue>,
) {
    let count = |arg: &Arg, state: &TemperModelState| -> usize {
        match arg {
            Arg::Lit(Literal::Int(n)) => usize::try_from(*n).unwrap_or(0),
            Arg::Var(name) => state.counters.get(name).copied().unwrap_or(0),
            Arg::Param(name) => match params.get(name) {
                Some(ParamValue::Count(n)) => *n,
                _ => 0,
            },
            Arg::Lit(_) => 0,
        }
    };
    for effect in effects {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = count(value, state);
                state.counters.insert(var.clone(), value);
            }
            ModelEffect::AddCounter { var, value } => {
                let delta = count(value, state);
                *state.counters.entry(var.clone()).or_insert(0) += delta;
            }
            ModelEffect::SubCounter { var, value } => {
                let delta = count(value, state);
                let entry = state.counters.entry(var.clone()).or_insert(0);
                *entry = entry.saturating_sub(delta);
            }
            ModelEffect::SetBool { var, value } => {
                let value = match value {
                    Arg::Lit(Literal::Bool(b)) => *b,
                    Arg::Var(name) => state.booleans.get(name).copied().unwrap_or(false),
                    Arg::Param(name) => matches!(params.get(name), Some(ParamValue::Bool(true))),
                    Arg::Lit(_) => false,
                };
                state.booleans.insert(var.clone(), value);
            }
            ModelEffect::ListAppend { var, value } => {
                let entry = state.lists.entry(var.clone()).or_default();
                let element = match value {
                    Arg::Lit(Literal::Str(text)) => text.clone(),
                    Arg::Param(name) => match params.get(name) {
                        Some(ParamValue::Str(ModelString::Literal(text))) => text.clone(),
                        _ => format!("{action_name}#{}", entry.len() + 1),
                    },
                    _ => format!("{action_name}#{}", entry.len() + 1),
                };
                entry.push(element);
            }
            ModelEffect::ListRemoveAt { var, index } => {
                let index = count(index, state);
                if let Some(entry) = state.lists.get_mut(var)
                    && index < entry.len()
                {
                    entry.remove(index);
                }
            }
        }
    }
}

/// Collect every `(list_var, value)` pair tested by `'value' in list_var`.
pub fn collect_list_contains_pairs(expr: &Expr, pairs: &mut BTreeSet<(String, String)>) {
    match expr {
        Expr::In {
            value: Operand::Lit(temper_spec::predicate::Literal::Str(value)),
            set: Set::Var(var),
            ..
        } => {
            pairs.insert((var.clone(), value.clone()));
        }
        Expr::Not(inner) => collect_list_contains_pairs(inner, pairs),
        Expr::And(parts) | Expr::Or(parts) => {
            for part in parts {
                collect_list_contains_pairs(part, pairs);
            }
        }
        Expr::Implies(a, b) => {
            collect_list_contains_pairs(a, pairs);
            collect_list_contains_pairs(b, pairs);
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "semantics_test.rs"]
mod tests;
