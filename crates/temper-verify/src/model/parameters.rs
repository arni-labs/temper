//! A move's chosen inputs, read in the state before its effects.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, ActionConstraint};
use temper_spec::predicate::{CmpOp, Env, Expr, Literal, Operand, Set, Val, eval};

use super::semantics::{Expansion, ModelEnv, evaluate_guard, param_assignments};
use super::strings::string_assignments;
use super::types::{ModelString, ParamValue, ResolvedTransition, TemperModel, TemperModelState};

/// State and parameters shared by guard and entry-rule evaluation.
pub(super) struct MoveEnv<'a> {
    pub(super) local: ModelEnv<'a>,
    pub(super) params: &'a BTreeMap<String, ParamValue>,
}

impl Env for MoveEnv<'_> {
    fn status(&self) -> Val<'_> {
        self.local.status()
    }

    fn var(&self, name: &str) -> Val<'_> {
        self.local.var(name)
    }

    fn param(&self, name: &str) -> Val<'_> {
        match self.params.get(name) {
            Some(ParamValue::Count(n)) => Val::Num(*n as f64),
            Some(ParamValue::Bool(value)) => Val::Bool(*value),
            Some(ParamValue::Str(value)) => value.val(),
            Some(ParamValue::Fresh) | None => Val::Unknown,
        }
    }

    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}

/// Whether these inputs can satisfy the move's guard and comparison
/// constraints. String nonempty constraints are decided when inputs are made.
pub(super) fn inputs_allow(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> bool {
    eval(
        &t.guard,
        &MoveEnv {
            local: ModelEnv {
                kinds: &model.var_kinds,
                state,
            },
            params,
        },
    )
    .may_hold()
}

/// Keep comparison constraints in the same predicate as the guard, so an
/// assignment cannot satisfy them independently. Nonempty is a string-input
/// property and is already enforced by `string_candidates`.
pub(super) fn guard_with_constraints(action: &Action) -> Expr {
    let mut parts = vec![action.guard.clone()];
    for constraint in &action.constraints {
        let op = match constraint {
            ActionConstraint::ParamEqualsField { .. } => CmpOp::Eq,
            ActionConstraint::ParamNotEqualsField { .. } => CmpOp::Ne,
            ActionConstraint::ParamGreaterThanField { .. } => CmpOp::Gt,
            ActionConstraint::ParamNonempty { .. } => continue,
        };
        parts.push(Expr::Compare {
            lhs: Operand::Param(constraint.param().to_string()),
            op,
            rhs: Operand::Var(constraint.field().expect("comparison field").to_string()),
        });
    }
    Expr::And(parts)
}

/// Effects can collapse inputs that a guard distinguishes. Explore through
/// every guard literal and local count, plus one distinct value per input:
/// comparisons can distinguish those values even when effects cannot.
pub(super) fn guard_count_bound(model: &TemperModel, t: &ResolvedTransition) -> usize {
    fn literal(value: &Literal) -> usize {
        match value {
            Literal::Int(n) => usize::try_from(*n).unwrap_or(0),
            _ => 0,
        }
    }
    fn operand(value: &Operand) -> usize {
        match value {
            Operand::Lit(value) => literal(value),
            _ => 0,
        }
    }
    fn largest(expr: &Expr) -> usize {
        match expr {
            Expr::Compare { lhs, rhs, .. } => operand(lhs).max(operand(rhs)),
            Expr::In { value, set, .. } => operand(value).max(match set {
                Set::List(items) => items.iter().map(literal).max().unwrap_or(0),
                Set::Var(_) => 0,
            }),
            Expr::And(parts) | Expr::Or(parts) => parts.iter().map(largest).max().unwrap_or(0),
            Expr::Implies(a, b) => largest(a).max(largest(b)),
            Expr::Not(inner) => largest(inner),
            _ => 0,
        }
    }
    model
        .counter_bounds
        .values()
        .chain(model.list_bounds.values())
        .copied()
        .fold(largest(&t.guard), usize::max)
        .saturating_add(t.params.len())
}

/// Visit chosen inputs before effects or state bounds are applied, stopping
/// when `visit` returns true. Exploration and local enablement share choices.
pub(super) fn any_inputs(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
    mut visit: impl FnMut(BTreeMap<String, ParamValue>) -> bool,
) -> bool {
    let strings = string_assignments(model, t, state, values, budget);
    for params in param_assignments(model, t) {
        for chosen in &strings {
            if !budget.take() {
                return false;
            }
            let mut params = params.clone();
            params.extend(
                chosen
                    .iter()
                    .map(|(k, v)| (k.clone(), ParamValue::Str(v.clone()))),
            );
            if visit(params) {
                return true;
            }
        }
    }
    false
}

/// Whether some allowed input makes the guard definitely true locally.
/// Related-status gates remain unknown: the record may still need to wait.
/// Effects going past an exploration bound do not disable a runtime move.
/// None means the input search exhausted its budget; it proves neither
/// enablement nor a deadlock.
pub(super) fn locally_enabled(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> Option<bool> {
    if model.entry_rules.is_empty() {
        return Some(evaluate_guard(&t.guard, &model.var_kinds, state));
    }
    let values = super::strings::string_values(std::iter::once(state));
    let budget = Expansion::for_model(model);
    let enabled = any_inputs(model, t, state, &values, &budget, |params| {
        eval(
            &t.guard,
            &MoveEnv {
                local: ModelEnv {
                    kinds: &model.var_kinds,
                    state,
                },
                params: &params,
            },
        )
        .must_hold()
    });
    (!budget.exhausted.get()).then_some(enabled)
}
