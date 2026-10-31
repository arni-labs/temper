//! String state and equality literals needed by the model's predicates.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Literal, Name, Operand, Set, VarKind};

use super::rule_asserts;

/// The string variables the verifier tracks: those `[[invariant]]`s and
/// `[[entry_rule]]`s read, their guards and constraint fields, plus
/// `also_tracked`, plus the string fields constraints tie to tracked parameters.
/// Specs without entry rules or string invariants explore as before.
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
    let predicates = rule_asserts(automaton).chain(
        automaton
            .actions
            .iter()
            .filter(|_| !automaton.entry_rules.is_empty())
            .map(|action| &action.guard),
    );
    for assert in predicates {
        assert.for_each_name(&mut |name| {
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

/// Concrete equality classes from every predicate. A local input may be tied
/// to a literal indirectly through related statuses, another variable or a
/// list, so sampling only literals directly compared to that variable loses
/// transitive equalities. Sharing the finite literal vocabulary is safe;
/// guards still decide which choices are executable.
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
    for action in &automaton.actions {
        visit(&action.guard, &mut literals);
    }
    for assert in rule_asserts(automaton) {
        visit(assert, &mut literals);
    }
    tracked
        .iter()
        .map(|var| (var.clone(), literals.clone()))
        .collect()
}
