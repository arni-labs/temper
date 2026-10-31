//! String variables and literal choices needed to build the verification model.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{ActionConstraint, ActionKind, Automaton, Effect};
use temper_spec::predicate::{Arg, CmpOp, Expr, Literal, Name, Operand, Set, VarKind};

use crate::model::types::StringParam;

/// The parameters of `action` that write a tracked string variable, or that
/// a constraint ties to one (`failed_commit` must equal `commit`), with the
/// action's constraints on each. The model needs the value of the second
/// kind to decide whether the record takes the action, though it writes
/// nothing it tracks. Route checks also track inputs appended to lists so
/// equality with a string field or another list element survives storage.
pub(super) fn string_params(
    action: &temper_spec::automaton::Action,
    tracked_strings: &BTreeSet<String>,
    checks_routes: bool,
) -> Vec<StringParam> {
    let tied = |name: &str| {
        action.constraints.iter().any(|c| {
            c.param() == name
                && matches!(
                    c,
                    ActionConstraint::ParamEqualsField { .. }
                        | ActionConstraint::ParamNotEqualsField { .. }
                )
                && c.field()
                    .is_some_and(|field| tracked_strings.contains(field))
        })
    };
    let appended: BTreeSet<&str> = action
        .effect
        .iter()
        .filter_map(|effect| match effect {
            Effect::Append {
                value: Arg::Param(param),
                ..
            } if checks_routes => Some(param.as_str()),
            _ => None,
        })
        .collect();
    action
        .params
        .iter()
        .map(|param| param.name())
        .filter(|name| tracked_strings.contains(*name) || tied(name) || appended.contains(*name))
        .map(|name| {
            let mut param = StringParam {
                name: name.to_string(),
                writes: tracked_strings.contains(name),
                nonempty: false,
                equals: Vec::new(),
                not_equals: Vec::new(),
            };
            for constraint in action.constraints.iter().filter(|c| c.param() == name) {
                match constraint {
                    ActionConstraint::ParamNonempty { .. } => param.nonempty = true,
                    ActionConstraint::ParamEqualsField { field, .. } => {
                        param.equals.push(field.clone());
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

/// The string variables the verifier tracks: those invariants read, and,
/// when proving `can_reach`, those guards read. A possible edge through an
/// untracked local value cannot establish a route. Also include `also_tracked`
/// and every string variable a route action constrains an input against: its
/// value decides what the tracked one may become. Specs without `can_reach`
/// keep their existing invariant-driven tracking.
pub(super) fn tracked_strings(
    automaton: &Automaton,
    also_tracked: &BTreeSet<String>,
) -> BTreeSet<String> {
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
    let checks_routes = automaton.liveness.iter().any(|l| !l.can_reach.is_empty());
    let guards = automaton
        .actions
        .iter()
        .filter(|a| checks_routes && a.kind != ActionKind::Output)
        .map(|a| &a.guard);
    let assertions = automaton.invariants.iter().map(|inv| &inv.assert);
    for expr in assertions.chain(guards) {
        expr.for_each_name(&mut |name| {
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
                if !checks_routes && !tracked.contains(constraint.param()) {
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
/// compares it with (`v == 'x'`, `v in ['x', 'y']`), also offered to its
/// constraint sources when checking `can_reach`. Route inputs share all
/// literal distinctions, since equality or membership can connect fields.
pub(super) fn string_literals(
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
    // A parameter can supply a tracked string and append to a list on the
    // same step. Its string choices must preserve the list's distinctions
    // too, before constraints propagate those choices to source fields.
    let list_literals = super::list_literals(automaton);
    for action in &automaton.actions {
        for effect in &action.effect {
            if let Effect::Append {
                list,
                value: Arg::Param(param),
            } = effect
                && let Some(literals) = list_literals.get(list)
            {
                out.entry(param.clone())
                    .or_default()
                    .extend(literals.iter().cloned());
            }
        }
    }
    // Equality and membership can carry a literal between fields without an
    // explicit input constraint. Give every route input the same literal
    // distinctions, including values initially stored or appended by effects.
    if automaton.liveness.iter().any(|l| !l.can_reach.is_empty()) {
        let mut literals: BTreeSet<String> = out.values().flatten().cloned().collect();
        literals.extend(list_literals.values().flatten().cloned());
        for effect in automaton.actions.iter().flat_map(|action| &action.effect) {
            if let Effect::Append {
                value: Arg::Lit(Literal::Str(text)),
                ..
            } = effect
            {
                literals.insert(text.clone());
            }
        }
        for state in &automaton.state {
            match &state.initial {
                temper_spec::automaton::Initial::String(text) => {
                    literals.insert(text.clone());
                }
                temper_spec::automaton::Initial::List(items) => {
                    literals.extend(items.iter().cloned());
                }
                _ => {}
            }
        }
        let inputs: BTreeSet<String> = automaton
            .actions
            .iter()
            .flat_map(|action| string_params(action, tracked, true))
            .map(|param| param.name)
            .chain(tracked.iter().cloned())
            .collect();
        for name in inputs {
            out.entry(name)
                .or_default()
                .extend(literals.iter().cloned());
        }
    }
    out
}
