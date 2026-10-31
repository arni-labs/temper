//! String variables and literal choices needed to build the verification model.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{ActionConstraint, Automaton, Effect, Initial};
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

/// The string variables the verifier tracks:
/// - those `[[invariant]]`s read, and `also_tracked`;
/// - those guards read or effects write that no action's parameter writes;
/// - and every string variable a constraint ties the parameter of a tracked
///   one to (its value decides what the tracked one may become), or a guard
///   or invariant compares with a tracked one (`x == y`).
///
/// `can_reach` rules also need every guard/effect string and constraint source:
/// an unmodeled value must not certify an impossible route.
///
/// Other strings stay unmodeled, and a guard that reads one may go either
/// way. A string no parameter writes holds only its initial value and the
/// effects' literals, so tracking it adds few states. A string a parameter
/// writes takes a fresh symbol at each write, and tracking one only guards
/// read multiplies the states to check: four such strings took a spec from
/// 137,137 states to past 10 million generated.
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
    let param_written: BTreeSet<&str> = automaton
        .actions
        .iter()
        .flat_map(|action| action.params.iter().map(|param| param.name()))
        .collect();
    let mut tracked: BTreeSet<String> = also_tracked
        .iter()
        .filter(|name| is_string(name))
        .cloned()
        .collect();
    let checks_routes = automaton.liveness.iter().any(|l| !l.can_reach.is_empty());
    for inv in &automaton.invariants {
        inv.assert.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
            {
                tracked.insert(var.to_string());
            }
        });
    }
    for action in &automaton.actions {
        action.guard.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
                && (checks_routes || !param_written.contains(var))
            {
                tracked.insert(var.to_string());
            }
        });
        for effect in &action.effect {
            if let temper_spec::predicate::Effect::Assign { var, .. } = effect
                && is_string(var)
                && (checks_routes || !param_written.contains(var.as_str()))
            {
                tracked.insert(var.clone());
            }
        }
    }
    let mut compared = Vec::new();
    for expr in automaton
        .invariants
        .iter()
        .map(|inv| &inv.assert)
        .chain(automaton.actions.iter().map(|action| &action.guard))
    {
        compared_strings(expr, &is_string, &mut compared);
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
        for (a, b) in &compared {
            if tracked.contains(a) || tracked.contains(b) {
                tracked.insert(a.clone());
                tracked.insert(b.clone());
            }
        }
        if tracked.len() == before {
            return tracked;
        }
    }
}

/// The pairs of string variables `expr` compares with each other (`x == y`,
/// `x != y`).
fn compared_strings(
    expr: &Expr,
    is_string: &impl Fn(&str) -> bool,
    out: &mut Vec<(String, String)>,
) {
    match expr {
        Expr::Not(inner) => compared_strings(inner, is_string, out),
        Expr::And(parts) | Expr::Or(parts) => {
            parts
                .iter()
                .for_each(|part| compared_strings(part, is_string, out));
        }
        Expr::Implies(a, b) => {
            compared_strings(a, is_string, out);
            compared_strings(b, is_string, out);
        }
        Expr::Compare {
            lhs: Operand::Var(a),
            op: CmpOp::Eq | CmpOp::Ne,
            rhs: Operand::Var(b),
        } if is_string(a) && is_string(b) => out.push((a.clone(), b.clone())),
        _ => {}
    }
}

/// For every tracked string variable, all initial, effect and comparison
/// literals any tracked string may hold (`v == 'x'`, `v in ['x', 'y']`).
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
    // Share initial, effect and comparison literals: an earlier input can
    // equal a value another field receives from a later effect.
    let mut written: BTreeSet<String> = out.values().flatten().cloned().collect();
    for state in &automaton.state {
        if tracked.contains(&state.name)
            && let Initial::String(value) = &state.initial
        {
            written.insert(value.clone());
        }
    }
    for effect in automaton.actions.iter().flat_map(|action| &action.effect) {
        if let Effect::Assign {
            value: Arg::Lit(Literal::Str(value)),
            ..
        } = effect
        {
            written.insert(value.clone());
        }
    }
    for var in tracked {
        out.insert(var.clone(), written.clone());
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
        // A field can be compared with status after its input is stored.
        // Every declared status is concrete, even without a quoted guard literal.
        literals.extend(automaton.automaton.states.iter().cloned());
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
                Initial::String(text) => {
                    literals.insert(text.clone());
                }
                Initial::List(items) => {
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
