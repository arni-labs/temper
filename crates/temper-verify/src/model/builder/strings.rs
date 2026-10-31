//! Tracked string fields and their finite literal choices.

use std::collections::{BTreeMap, BTreeSet};
use temper_spec::automaton::{Automaton, Initial};
use temper_spec::predicate::{Arg, CmpOp, Expr, Literal, Name, Operand, Set, VarKind};

/// The string variables the verifier tracks: those guards or `[[invariant]]`s read or effects write,
/// plus `also_tracked`, plus every string variable a constraint ties the
/// parameter of a tracked one to (its value decides what the tracked one may
/// become). Other strings stay unmodeled.
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
    for expr in automaton
        .invariants
        .iter()
        .map(|inv| &inv.assert)
        .chain(automaton.actions.iter().map(|action| &action.guard))
    {
        expr.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
            {
                tracked.insert(var.to_string());
            }
        });
    }
    // An effect-only string has no parameter choices: its reachable values
    // are exactly its initial value and the finitely many effect literals.
    // Parameter-written strings keep the existing equality-symbol abstraction
    // so arbitrary input stays covered without enumerating unbounded strings.
    for action in &automaton.actions {
        for effect in &action.effect {
            if let temper_spec::predicate::Effect::Assign { var, .. } = effect
                && is_string(var)
            {
                tracked.insert(var.clone());
            }
        }
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
    let mut written = BTreeSet::new();
    for sv in &automaton.state {
        if tracked.contains(&sv.name)
            && let Initial::String(value) = &sv.initial
        {
            written.insert(value.clone());
        }
    }
    for action in &automaton.actions {
        for effect in &action.effect {
            if let temper_spec::predicate::Effect::Assign {
                value: Arg::Lit(Literal::Str(value)),
                ..
            } = effect
            {
                written.insert(value.clone());
            }
        }
    }
    for action in &automaton.actions {
        visit(&action.guard, tracked, &mut out);
    }
    for inv in &automaton.invariants {
        visit(&inv.assert, tracked, &mut out);
    }
    // An earlier param may equal a literal compared with another field later
    // (x == y && y == 'special'). Share comparison literals as well as initial
    // and effect values: a fresh symbol cannot equal a concrete literal.
    // This stays finite; fresh symbols still cover all other parameter inputs,
    // and effect-only fields still take only their initial and written values.
    written.extend(out.values().flatten().cloned());
    for var in tracked {
        out.insert(var.clone(), written.clone());
    }
    out
}
