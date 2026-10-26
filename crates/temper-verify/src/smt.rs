//! SMT symbolic verification (Level 0 of the verification cascade).
//!
//! Uses the Z3 SMT solver to verify properties algebraically without
//! enumerating states:
//!
//! 1. **Guard satisfiability** — Encode each guard as a Z3 formula over
//!    integer counters (0..max) and boolean variables. Check SAT: if UNSAT,
//!    the guard is dead code (the action can never fire).
//!
//! 2. **Invariant induction** — For each (invariant, transition) pair:
//!    assume `invariant(S) ∧ guard(S) ∧ status ∈ from_states`, apply
//!    effects to get S', prove `invariant(S')` by checking that its
//!    negation is UNSAT.
//!
//! 3. **Unreachable state detection** — BFS from initial state through
//!    transition targets to find states that can never be reached.

use std::collections::{BTreeMap, BTreeSet};

use z3::ast::{Bool, Int};
use z3::{SatResult, Solver};

mod encoding;
use encoding::*;

use temper_spec::predicate::{Arg, Expr, Literal, Operand, Set};

use crate::model::builder::build_model_from_ioa;
use crate::model::types::{ModelEffect, ResolvedTransition, TemperModel};

/// Result of symbolic verification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmtResult {
    /// For each action, whether its guard is satisfiable (can ever fire).
    pub guard_satisfiability: Vec<(String, bool)>,
    /// For each invariant, whether it is inductively maintained by all transitions.
    pub inductive_invariants: Vec<(String, bool)>,
    /// States that cannot be reached from the initial state.
    pub unreachable_states: Vec<String>,
    /// Whether symbolic checks rely on bounded/abstract encodings.
    pub approximate: bool,
    /// Human-readable approximation notes for downstream reporting.
    pub approximation_notes: Vec<String>,
    /// Whether all checks passed (no dead guards, all invariants inductive).
    pub all_passed: bool,
}

/// Run symbolic verification on an IOA spec using the Z3 SMT solver.
///
/// This is the Level 0 entry point. It checks:
/// 1. Guard satisfiability: is there any state in which each guard can fire?
/// 2. Invariant induction: does each invariant hold after every transition?
/// 3. Unreachable states: can each declared state be reached?
pub fn verify_symbolic(ioa_toml: &str, max_counter: usize) -> SmtResult {
    let model = build_model_from_ioa(ioa_toml, max_counter)
        .expect("SMT: IOA spec should have been validated before symbolic verification");
    let approximation_notes = approximation_notes(&model);
    let approximate = !approximation_notes.is_empty();

    let guard_sat = check_guard_satisfiability(&model, max_counter);
    let inductive = check_invariant_induction(&model, max_counter);
    let unreachable = check_unreachable_states(&model);

    // Unreachable states are warnings, not failures — specs may declare states
    // that are only reachable through composition or external actions.
    let all_passed = guard_sat.iter().all(|(_, sat)| *sat) && inductive.iter().all(|(_, ind)| *ind);

    SmtResult {
        guard_satisfiability: guard_sat,
        inductive_invariants: inductive,
        unreachable_states: unreachable,
        approximate,
        approximation_notes,
        all_passed,
    }
}

fn approximation_notes(model: &TemperModel) -> Vec<String> {
    let cross_entity_guard_count = model
        .transitions
        .iter()
        .filter(|transition| reads_related_entities(&transition.guard))
        .count();
    if cross_entity_guard_count == 0 {
        return Vec::new();
    }

    vec![format!(
        "{cross_entity_guard_count} transition(s) use abstract cross-entity guards; single-entity SMT excludes them from local induction and reachability"
    )]
}

/// Whether a guard reads a related entity's status (not modeled here).
fn reads_related_entities(guard: &Expr) -> bool {
    !guard.cross_refs().is_empty()
}

fn concrete_transitions(model: &TemperModel) -> impl Iterator<Item = &ResolvedTransition> {
    model
        .transitions
        .iter()
        .filter(|transition| !reads_related_entities(&transition.guard))
}

// ---------------------------------------------------------------------------
// Guard satisfiability
// ---------------------------------------------------------------------------

/// For each transition, encode its guard as a Z3 formula and check SAT.
///
/// A guard is satisfiable if there exists an assignment of counter values
/// (0..max_counter) and boolean values that makes the guard true.
fn check_guard_satisfiability(model: &TemperModel, max_counter: usize) -> Vec<(String, bool)> {
    model
        .transitions
        .iter()
        .map(|t| {
            let solver = Solver::new();

            // Check that at least one from_state exists in the state space
            if !t.from_states.is_empty() {
                let has_valid_from = t.from_states.iter().any(|s| model.states.contains(s));
                if !has_valid_from {
                    return (t.name.clone(), false);
                }
            }

            // Create Z3 variables for each counter, bounded [0, max_counter]
            let counter_vars = make_counter_vars(model, &solver, max_counter);
            let bool_vars = make_bool_vars(model);
            let list_vars = make_list_vars(model, &solver, max_counter);
            let status_var = make_status_var(model, &solver);

            if !t.from_states.is_empty() {
                let from_formula = encode_state_membership(&status_var, &t.from_states, model);
                solver.assert(&from_formula);
            }

            // Encode the guard as a Z3 formula and assert it
            let symbols = Symbols {
                counters: counter_vars.into_iter().collect(),
                bools: bool_vars.into_iter().collect(),
                strings: make_string_vars(model, ""),
                lists: &list_vars,
                status: status_var,
                prefix: "",
            };
            solver.assert(encode(&t.guard, &symbols, model));

            let sat = matches!(solver.check(), SatResult::Sat);
            (t.name.clone(), sat)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Invariant induction
// ---------------------------------------------------------------------------

/// For each invariant, check that the transitions entering its states
/// preserve it.
///
/// L0 scope (unchanged by the unified grammar): an invariant written as
/// `status in [W...] => P` is checked on every concrete transition whose
/// target is in `W`: assume `P` in the pre-state, apply the effects, prove
/// `P` in the post-state. Guards and other variables are not constrained, so
/// this is a fast early filter; the model checker (L1) is the full proof.
/// Invariants without a `status in [...] =>` prefix, and invariants over
/// values the model does not track, are left to L1. Status membership and
/// `terminal` states are checked structurally.
fn check_invariant_induction(model: &TemperModel, max_counter: usize) -> Vec<(String, bool)> {
    let mut results = vec![(
        "TypeInvariant".to_string(),
        concrete_transitions(model).all(|t| {
            t.to_state
                .as_ref()
                .map(|s| model.states.contains(s))
                .unwrap_or(true)
        }),
    )];
    for inv in &model.invariants {
        let inductive = match split_trigger(&inv.assert) {
            Some((trigger, body)) if models(body, model) => concrete_transitions(model)
                .filter(|t| t.to_state.as_ref().is_some_and(|to| trigger.contains(to)))
                .all(|t| preserved_by(model, body, t, max_counter)),
            _ => true,
        };
        results.push((inv.name.clone(), inductive));
    }
    for state in &model.terminal {
        let closed = !concrete_transitions(model)
            .any(|t| t.from_states.contains(state) || t.from_states.is_empty());
        results.push((format!("Terminal({state})"), closed));
    }
    results
}

/// `status in [W...] => P` as `(W, P)`.
fn split_trigger(assert: &Expr) -> Option<(Vec<String>, &Expr)> {
    let Expr::Implies(lhs, body) = assert else {
        return None;
    };
    let Expr::In {
        value: Operand::Status,
        set: Set::List(items),
        negated: false,
    } = lhs.as_ref()
    else {
        return None;
    };
    let states = items
        .iter()
        .map(|item| match item {
            Literal::Str(state) => Some(state.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((states, body))
}

/// Whether transition `t` preserves `body`: `body(pre) ∧ post = effects(pre)
/// ∧ ¬body(post)` is unsatisfiable.
fn preserved_by(
    model: &TemperModel,
    body: &Expr,
    t: &ResolvedTransition,
    max_counter: usize,
) -> bool {
    let solver = Solver::new();
    let zero = Int::from_i64(0);
    let max_val = Int::from_i64(max_counter as i64);
    let lists = ListSymbolicVars::default();

    let mut pre_counters = BTreeMap::new();
    for name in model.initial_counters.keys() {
        let pre = Int::new_const(format!("{name}_pre"));
        solver.assert(pre.ge(&zero));
        solver.assert(pre.le(&max_val));
        pre_counters.insert(name.clone(), pre);
    }
    let mut pre_bools = BTreeMap::new();
    for name in model.initial_booleans.keys() {
        pre_bools.insert(name.clone(), Bool::new_const(format!("{name}_pre")));
    }
    // Apply the effects in order, as the runtime does; `params.p` is a fresh
    // unknown (a non-negative integer or a boolean).
    let mut post_counters = pre_counters.clone();
    let mut post_bools = pre_bools.clone();
    let pre_strings = make_string_vars(model, "pre:");
    let mut post_strings = pre_strings.clone();
    let count = |arg: &Arg, counters: &BTreeMap<String, Int>| -> Int {
        match arg {
            Arg::Lit(Literal::Int(n)) => Int::from_i64(*n),
            Arg::Var(name) => counters.get(name).cloned().unwrap_or_else(|| zero.clone()),
            Arg::Param(name) => {
                let param = Int::new_const(format!("param:{name}"));
                solver.assert(param.ge(&zero));
                param
            }
            Arg::Lit(_) => zero.clone(),
        }
    };
    for effect in &t.effects {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = count(value, &post_counters);
                post_counters.insert(var.clone(), value);
            }
            ModelEffect::AddCounter { var, value } => {
                let delta = count(value, &post_counters);
                if let Some(post) = post_counters.get_mut(var) {
                    *post = Int::add(&[&*post, &delta]);
                }
            }
            ModelEffect::SubCounter { var, value } => {
                // Runtime semantics are saturating: max(counter - delta, 0).
                let delta = count(value, &post_counters);
                if let Some(post) = post_counters.get_mut(var) {
                    let dec = Int::sub(&[&*post, &delta]);
                    *post = post.gt(&delta).ite(&dec, &zero);
                }
            }
            ModelEffect::SetBool { var, value } => {
                let value = match value {
                    Arg::Lit(Literal::Bool(b)) => Bool::from_bool(*b),
                    Arg::Var(name) => post_bools
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| Bool::from_bool(false)),
                    Arg::Param(name) => Bool::new_const(format!("param:{name}")),
                    Arg::Lit(_) => Bool::from_bool(false),
                };
                post_bools.insert(var.clone(), value);
            }
            ModelEffect::SetString { var, value } => {
                post_strings.insert(
                    var.clone(),
                    string_literal(value, model).expect("effect literal is in the model"),
                );
            }
            ModelEffect::ListAppend { .. } | ModelEffect::ListRemoveAt { .. } => {}
        }
    }
    // Params are written after effects in production. Preserve that order
    // when a move both assigns a literal and supplies the field as a param.
    for param in &t.string_params {
        let value = Int::new_const(format!("param:{}", param.name));
        if param.nonempty {
            solver.assert(value.eq(Int::from_i64(0)).not());
        }
        if let Some(field) = param
            .equals
            .as_ref()
            .and_then(|field| pre_strings.get(field))
        {
            solver.assert(value.eq(field));
        }
        for field in &param.not_equals {
            if let Some(field) = pre_strings.get(field) {
                solver.assert(value.eq(field).not());
            }
        }
        post_strings.insert(param.name.clone(), value);
    }
    let pre_status = make_status_var(model, &solver);
    let post_status = match t
        .to_state
        .as_ref()
        .and_then(|to| model.states.iter().position(|s| s == to))
    {
        Some(idx) => Int::from_i64(idx as i64),
        None => pre_status.clone(),
    };

    let pre = Symbols {
        counters: pre_counters,
        bools: pre_bools,
        strings: pre_strings,
        lists: &lists,
        status: pre_status,
        prefix: "pre:",
    };
    let post = Symbols {
        counters: post_counters,
        bools: post_bools,
        strings: post_strings,
        lists: &lists,
        status: post_status,
        prefix: "post:",
    };
    solver.assert(encode(body, &pre, model));
    solver.assert(encode(body, &post, model).not());
    !matches!(solver.check(), SatResult::Sat)
}

// ---------------------------------------------------------------------------
// Unreachable state detection (graph-based, no Z3 needed)
// ---------------------------------------------------------------------------

/// Check which states are unreachable from the initial state.
fn check_unreachable_states(model: &TemperModel) -> Vec<String> {
    let mut reachable: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = vec![&model.initial_status];

    while let Some(state) = queue.pop() {
        if !reachable.insert(state) {
            continue;
        }
        for t in &model.transitions {
            if reads_related_entities(&t.guard) {
                continue;
            }
            let can_fire_from =
                t.from_states.is_empty() || t.from_states.iter().any(|s| s == state);
            if can_fire_from
                && let Some(to) = &t.to_state
                && !reachable.contains(to.as_str())
            {
                queue.push(to);
            }
        }
    }

    model
        .states
        .iter()
        .filter(|s| !reachable.contains(s.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests;
