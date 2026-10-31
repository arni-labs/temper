//! String state and equality literals needed by the model's predicates.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{ActionKind, Automaton};
use temper_spec::predicate::{CmpOp, Expr, Literal, Name, Operand, Set, VarKind};

use super::rule_asserts;

/// The string variables the verifier tracks: those `[[invariant]]`s and
/// `[[entry_rule]]`s read, their covered guards and constraint fields, plus
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
    let covered_actions = automaton.actions.iter().filter(|action| {
        action.kind != ActionKind::Output
            && automaton.entry_rules.iter().any(|rule| {
                action.to.as_ref() == Some(&rule.place)
                    && automaton.automaton.states.iter().any(|from| {
                        (action.from.is_empty() || action.from.contains(from))
                            && if rule.from.is_empty() {
                                from != &rule.place
                            } else {
                                rule.from.contains(from)
                            }
                    })
            })
    });
    for action in covered_actions.clone() {
        // Even a rule with no local reads must not report a move whose
        // parameter constraints have no solution in the pre-state.
        for field in action.constraints.iter().filter_map(|c| c.field()) {
            if is_string(field) {
                tracked.insert(field.to_string());
            }
        }
    }
    let predicates = rule_asserts(automaton).chain(covered_actions.map(|action| &action.guard));
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

/// For each tracked string variable, the literals a guard or rule compares
/// it with (`v == 'x'`, `v in ['x', 'y']`).
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
    for assert in rule_asserts(automaton) {
        visit(assert, tracked, &mut out);
    }
    out
}
