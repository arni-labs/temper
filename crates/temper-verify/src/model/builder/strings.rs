//! String state and equality literals needed by the model's predicates and effects.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, Initial};
use temper_spec::predicate::{Arg, CmpOp, Effect, Expr, Literal, Name, Operand, Set, VarKind};

use super::rule_asserts;

/// The string variables the verifier tracks:
/// - those `[[invariant]]`s and `[[entry_rule]]`s read, constraint fields when
///   the type has entry rules, and `also_tracked`;
/// - those guards read or effects write that no action's parameter writes,
///   and every string a move into a place with an `[[entry_rule]]` reads in
///   its guard: the rule claims what that guard guarantees, so the guard is
///   modeled exactly;
/// - and every string variable a constraint ties the parameter of a tracked
///   one to (its value decides what the tracked one may become), or a guard
///   or rule compares with a tracked one (`x == y`).
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
    for action in automaton
        .actions
        .iter()
        .filter(|_| !automaton.entry_rules.is_empty())
    {
        // Both the entry and its prefix moves need executable inputs, even
        // when the assertion has no local reads.
        for field in action.constraints.iter().filter_map(|c| c.field()) {
            if is_string(field) {
                tracked.insert(field.to_string());
            }
        }
    }
    for assert in rule_asserts(automaton) {
        assert.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
            {
                tracked.insert(var.to_string());
            }
        });
    }
    for action in &automaton.actions {
        let enters_a_rule = automaton
            .entry_rules
            .iter()
            .any(|rule| action.to.as_deref() == Some(rule.place.as_str()));
        action.guard.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
                && (enters_a_rule || !param_written.contains(var))
            {
                tracked.insert(var.to_string());
            }
        });
        for effect in &action.effect {
            if let Effect::Assign { var, .. } = effect
                && is_string(var)
                && !param_written.contains(var.as_str())
            {
                tracked.insert(var.clone());
            }
        }
    }
    let mut compared = Vec::new();
    for expr in rule_asserts(automaton).chain(automaton.actions.iter().map(|action| &action.guard))
    {
        compared_strings(expr, &is_string, &mut compared);
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

/// Concrete equality classes from every predicate and string-writing effect.
/// A local input may be tied to a literal indirectly through related statuses,
/// another variable or a list, including a value appended by the same move, so
/// sampling only predicate literals loses transitive equalities. Sharing the
/// finite literal vocabulary is safe; guards still decide which choices are
/// executable.
pub(super) fn string_literals(
    automaton: &Automaton,
    tracked: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<String>> {
    fn note(operand: &Operand, out: &mut BTreeSet<String>) {
        if let Operand::Lit(Literal::Str(text)) = operand {
            out.insert(text.clone());
        }
    }
    fn visit(expr: &Expr, out: &mut BTreeSet<String>) {
        match expr {
            Expr::Not(inner) => visit(inner, out),
            Expr::And(parts) | Expr::Or(parts) => {
                parts.iter().for_each(|part| visit(part, out));
            }
            Expr::Implies(a, b) => {
                visit(a, out);
                visit(b, out);
            }
            Expr::Compare { lhs, rhs, .. } => {
                note(lhs, out);
                note(rhs, out);
            }
            Expr::In { value, set, .. } => {
                note(value, out);
                if let Set::List(items) = set {
                    for item in items {
                        if let Literal::Str(text) = item {
                            out.insert(text.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut literals = BTreeSet::new();
    for sv in &automaton.state {
        if tracked.contains(&sv.name)
            && let Initial::String(value) = &sv.initial
        {
            literals.insert(value.clone());
        }
    }
    for action in &automaton.actions {
        visit(&action.guard, &mut literals);
        for effect in &action.effect {
            if let Effect::Append {
                value: Arg::Lit(Literal::Str(text)),
                ..
            }
            | Effect::Assign {
                value: Arg::Lit(Literal::Str(text)),
                ..
            } = effect
            {
                literals.insert(text.clone());
            }
        }
    }
    for assert in rule_asserts(automaton) {
        visit(assert, &mut literals);
    }
    tracked
        .iter()
        .map(|var| (var.clone(), literals.clone()))
        .collect()
}
