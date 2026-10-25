//! Model builder: constructs a `TemperModel` directly from I/O Automaton specifications.
//!
//! Uses the shared translation layer in `temper-spec` for guard/effect translation,
//! then converts to verification-specific types. Runtime-only effects (dispatch,
//! schedule, spawn) are filtered out; parameter-driven effects are kept and
//! explored over every candidate parameter value. Cross-entity guards are kept
//! as abstract guards so single-entity checks do not silently treat them as
//! locally enabled.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{
    ActionConstraint, ActionKind, Automaton, Initial, ResolvedEffect, translate_actions,
};
use temper_spec::predicate::{Arg, CmpOp, Expr, Literal, Name, Operand, ParamKind, Set, VarKind};

use super::types::{
    LivenessKind, ModelEffect, ResolvedInvariant, ResolvedLiveness, ResolvedTransition,
    StringParam, TemperModel,
};

/// Build a `TemperModel` from I/O Automaton TOML source.
///
/// This is the sole entry point. The IOA format has explicit guards and effects,
/// so the `Automaton` is translated directly — no intermediate representation.
///
/// Returns an error if the IOA TOML fails to parse.
pub fn build_model_from_ioa(ioa_toml: &str, max_counter: usize) -> Result<TemperModel, String> {
    let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
        .map_err(|e| format!("failed to parse I/O Automaton TOML: {e}"))?;
    Ok(build_model_from_automaton(&automaton, max_counter))
}

/// Build a `TemperModel` directly from a parsed [`Automaton`].
pub fn build_model_from_automaton(automaton: &Automaton, max_counter: usize) -> TemperModel {
    build_model_tracking(automaton, max_counter, &BTreeSet::new())
}

/// [`build_model_from_automaton`], also tracking the string variables in
/// `also_tracked`: those another entity's invariant reads through
/// `Type[id].field`, or a reaction copies into a tracked variable.
pub fn build_model_tracking(
    automaton: &Automaton,
    max_counter: usize,
    also_tracked: &BTreeSet<String>,
) -> TemperModel {
    let states = automaton.automaton.states.clone();
    let initial_status = automaton.automaton.initial.clone();

    // Extract initial values from [[state]] declarations.
    let mut initial_counters = BTreeMap::new();
    let mut initial_booleans = BTreeMap::new();
    let mut initial_lists = BTreeMap::new();
    let mut counter_bounds = BTreeMap::new();

    for sv in &automaton.state {
        match &sv.initial {
            Initial::Counter(n) => {
                initial_counters.insert(sv.name.clone(), *n);
                counter_bounds.insert(sv.name.clone(), max_counter);
            }
            Initial::Bool(b) => {
                initial_booleans.insert(sv.name.clone(), *b);
            }
            Initial::List(items) => {
                initial_lists.insert(sv.name.clone(), items.clone());
            }
            // Strings and ints are not modeled.
            Initial::String(_) | Initial::Int(_) => {}
        }
    }

    let tracked_strings = tracked_strings(automaton, also_tracked);
    let initial_strings: BTreeMap<String, String> = automaton
        .state
        .iter()
        .filter(|sv| tracked_strings.contains(&sv.name))
        .map(|sv| {
            let value = match &sv.initial {
                Initial::String(text) => text.clone(),
                _ => String::new(),
            };
            (sv.name.clone(), value)
        })
        .collect();
    let transitions = resolve_transitions(automaton, &tracked_strings);
    // A counter written with a literal larger than the exploration bound is
    // explored up to that literal, so the write is not pruned away.
    for t in &transitions {
        for effect in &t.effects {
            if let ModelEffect::SetCounter {
                var,
                value: Arg::Lit(Literal::Int(n)),
            }
            | ModelEffect::AddCounter {
                var,
                value: Arg::Lit(Literal::Int(n)),
            } = effect
                && let (Some(bound), Ok(n)) = (counter_bounds.get_mut(var), usize::try_from(*n))
            {
                *bound = (*bound).max(n);
            }
        }
    }
    for (name, initial) in &initial_counters {
        if let Some(bound) = counter_bounds.get_mut(name) {
            *bound = (*bound).max(*initial);
        }
    }
    let invariants = resolve_invariants(automaton);
    let liveness = resolve_liveness(automaton);
    let var_kinds = automaton
        .state
        .iter()
        .map(|sv| (sv.name.clone(), sv.var_type.kind()))
        .collect();

    TemperModel {
        states,
        transitions,
        invariants,
        liveness,
        terminal: automaton.automaton.terminal.clone(),
        var_kinds,
        initial_status,
        initial_counters,
        initial_booleans,
        initial_lists,
        string_literals: string_literals(automaton, &tracked_strings),
        initial_strings,
        tracked_strings,
        counter_bounds,
        default_max_counter: max_counter,
        list_literals: list_literals(automaton),
        limit: None,
    }
}

/// `'x' in list` literals per list, across every guard and invariant.
fn list_literals(automaton: &Automaton) -> BTreeMap<String, BTreeSet<String>> {
    let mut pairs = BTreeSet::new();
    for action in &automaton.actions {
        super::semantics::collect_list_contains_pairs(&action.guard, &mut pairs);
    }
    for inv in &automaton.invariants {
        super::semantics::collect_list_contains_pairs(&inv.assert, &mut pairs);
    }
    let mut literals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (list, value) in pairs {
        literals.entry(list).or_default().insert(value);
    }
    literals
}

/// Translate IOA actions into resolved transitions using the shared translation layer.
fn resolve_transitions(
    automaton: &Automaton,
    tracked_strings: &BTreeSet<String>,
) -> Vec<ResolvedTransition> {
    // `translate_actions` keeps every action but outputs, in order.
    let declared = automaton
        .actions
        .iter()
        .filter(|a| a.kind != ActionKind::Output);
    translate_actions(automaton)
        .into_iter()
        .zip(declared)
        .map(|(a, action)| {
            let effects: Vec<ModelEffect> =
                a.effects.into_iter().filter_map(convert_effect).collect();
            let params = effect_params(&effects);
            ResolvedTransition {
                name: a.name,
                from_states: a.from_states,
                to_state: a.to_state,
                guard: a.guard,
                effects,
                params,
                string_params: string_params(action, tracked_strings),
            }
        })
        .collect()
}

/// The parameters of `action` that write a tracked string variable, with the
/// action's constraints on each.
fn string_params(
    action: &temper_spec::automaton::Action,
    tracked_strings: &BTreeSet<String>,
) -> Vec<StringParam> {
    action
        .params
        .iter()
        .map(|param| param.name())
        .filter(|name| tracked_strings.contains(*name))
        .map(|name| {
            let mut param = StringParam {
                name: name.to_string(),
                nonempty: false,
                equals: None,
                not_equals: Vec::new(),
            };
            for constraint in action.constraints.iter().filter(|c| c.param() == name) {
                match constraint {
                    ActionConstraint::ParamNonempty { .. } => param.nonempty = true,
                    ActionConstraint::ParamEqualsField { field, .. } => {
                        param.equals = Some(field.clone());
                    }
                    ActionConstraint::ParamNotEqualsField { field, .. } => {
                        param.not_equals.push(field.clone());
                    }
                    _ => {}
                }
            }
            param
        })
        .collect()
}

/// The string variables the verifier tracks: those `[[invariant]]`s read,
/// plus `also_tracked`, plus every string variable a constraint ties the
/// parameter of a tracked one to (its value decides what the tracked one may
/// become). Strings no invariant can see stay unmodeled, so a spec whose
/// invariants read none explores exactly as before.
fn tracked_strings(automaton: &Automaton, also_tracked: &BTreeSet<String>) -> BTreeSet<String> {
    let kinds: BTreeMap<&str, VarKind> = automaton
        .state
        .iter()
        .map(|sv| (sv.name.as_str(), sv.var_type.kind()))
        .collect();
    let is_string = |name: &str| kinds.get(name) == Some(&VarKind::Str);
    let mut tracked: BTreeSet<String> = also_tracked
        .iter()
        .filter(|name| is_string(name))
        .cloned()
        .collect();
    for inv in &automaton.invariants {
        inv.assert.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
            {
                tracked.insert(var.to_string());
            }
        });
    }
    loop {
        let before = tracked.len();
        for action in &automaton.actions {
            for constraint in &action.constraints {
                if !tracked.contains(constraint.param()) {
                    continue;
                }
                if let Some(field) = constraint.field()
                    && is_string(field)
                {
                    tracked.insert(field.to_string());
                }
            }
        }
        if tracked.len() == before {
            return tracked;
        }
    }
}

/// For each tracked string variable, the literals a guard or invariant
/// compares it with (`v == 'x'`, `v in ['x', 'y']`).
fn string_literals(
    automaton: &Automaton,
    tracked: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<String>> {
    fn visit(
        expr: &Expr,
        tracked: &BTreeSet<String>,
        out: &mut BTreeMap<String, BTreeSet<String>>,
    ) {
        match expr {
            Expr::Not(inner) => visit(inner, tracked, out),
            Expr::And(parts) | Expr::Or(parts) => {
                parts.iter().for_each(|part| visit(part, tracked, out));
            }
            Expr::Implies(a, b) => {
                visit(a, tracked, out);
                visit(b, tracked, out);
            }
            Expr::Compare {
                lhs,
                op: CmpOp::Eq | CmpOp::Ne,
                rhs,
            } => {
                for (var, lit) in [(lhs, rhs), (rhs, lhs)] {
                    if let (Operand::Var(var), Operand::Lit(Literal::Str(text))) = (var, lit)
                        && tracked.contains(var)
                    {
                        out.entry(var.clone()).or_default().insert(text.clone());
                    }
                }
            }
            Expr::In {
                value: Operand::Var(var),
                set: Set::List(items),
                ..
            } if tracked.contains(var) => {
                for item in items {
                    if let Literal::Str(text) = item {
                        out.entry(var.clone()).or_default().insert(text.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    for action in &automaton.actions {
        visit(&action.guard, tracked, &mut out);
    }
    for inv in &automaton.invariants {
        visit(&inv.assert, tracked, &mut out);
    }
    out
}

/// Convert a state [`ResolvedEffect`] to the verification [`ModelEffect`];
/// runtime-only effects (dispatch, schedule, spawn) have no model.
fn convert_effect(effect: ResolvedEffect) -> Option<ModelEffect> {
    Some(match effect {
        ResolvedEffect::SetCounter { var, value } => ModelEffect::SetCounter { var, value },
        ResolvedEffect::AddCounter { var, value } => ModelEffect::AddCounter { var, value },
        ResolvedEffect::SubCounter { var, value } => ModelEffect::SubCounter { var, value },
        ResolvedEffect::SetBool { var, value } => ModelEffect::SetBool { var, value },
        ResolvedEffect::ListAppend { var, value } => ModelEffect::ListAppend { var, value },
        ResolvedEffect::ListRemoveAt { var, index } => ModelEffect::ListRemoveAt { var, index },
        ResolvedEffect::Dispatch(_)
        | ResolvedEffect::Schedule { .. }
        | ResolvedEffect::ScheduleAt { .. }
        | ResolvedEffect::Spawn { .. } => return None,
    })
}

/// The kind of every `params.p` the effects read (checked consistent at load).
pub(crate) fn effect_params(effects: &[ModelEffect]) -> BTreeMap<String, ParamKind> {
    let mut params = BTreeMap::new();
    for effect in effects {
        let (arg, kind) = match effect {
            ModelEffect::SetCounter { value, .. }
            | ModelEffect::AddCounter { value, .. }
            | ModelEffect::SubCounter { value, .. } => (value, ParamKind::Count),
            ModelEffect::SetBool { value, .. } => (value, ParamKind::Bool),
            ModelEffect::ListAppend { value, .. } => (value, ParamKind::Str),
            ModelEffect::ListRemoveAt { index, .. } => (index, ParamKind::Count),
        };
        if let Arg::Param(name) = arg {
            params.insert(name.clone(), kind);
        }
    }
    params
}

/// Translate IOA invariants into resolved invariants. Status membership
/// (`TypeInvariant`) is checked by every backend separately.
fn resolve_invariants(automaton: &Automaton) -> Vec<ResolvedInvariant> {
    automaton
        .invariants
        .iter()
        .map(|inv| ResolvedInvariant {
            name: inv.name.clone(),
            assert: inv.assert.clone(),
        })
        .collect()
}

/// Translate IOA liveness properties into resolved liveness.
fn resolve_liveness(automaton: &Automaton) -> Vec<ResolvedLiveness> {
    automaton
        .liveness
        .iter()
        .map(|l| {
            let kind = if !l.reaches.is_empty() {
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: l.reaches.clone(),
                }
            } else if l.has_actions == Some(true) {
                LivenessKind::NoDeadlock {
                    from: l.from.clone(),
                }
            } else {
                // Default: treat as reachability with empty targets (trivially true)
                LivenessKind::ReachesState {
                    from: l.from.clone(),
                    targets: vec![],
                }
            };
            ResolvedLiveness {
                name: l.name.clone(),
                kind,
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "builder_test.rs"]
mod tests;
