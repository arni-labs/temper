//! String state and equality literals needed by the model's predicates and effects.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, Initial};
use temper_spec::predicate::{Arg, CmpOp, Effect, Expr, Literal, Name, Operand, Set, VarKind};

use super::rule_asserts;

/// The string variables the verifier tracks:
/// - those `[[invariant]]`s and `[[entry_rule]]`s read, constraint fields when
///   the type has entry rules, and `also_tracked`;
/// - those guards read or effects write that no action's parameter writes;
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
        action.guard.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && is_string(var)
                && !param_written.contains(var)
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

/// What a string may be compared with or copied into: a field (or the move
/// parameter of the same name that writes it), this record's status, or a
/// field of a related record (`Type[id].status`, `Type[id].field`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Term {
    Var(String),
    Status,
    Related(String, String),
}

fn term(operand: &Operand) -> Option<Term> {
    match operand {
        Operand::Var(name) | Operand::Param(name) => Some(Term::Var(name.clone())),
        Operand::Status => Some(Term::Status),
        Operand::CrossStatus { entity_type, .. } => {
            Some(Term::Related(entity_type.clone(), "status".to_string()))
        }
        Operand::CrossField {
            entity_type, field, ..
        } => Some(Term::Related(entity_type.clone(), field.clone())),
        Operand::Len(_) | Operand::Lit(_) => None,
    }
}

/// The literals worth choosing for the string inputs, from equality classes.
/// Two terms share a class when a predicate compares them (`x == y`,
/// `x == status`, `x in list`), an effect copies one into the other, or a
/// constraint ties a parameter to a field. A class holds the literals its
/// terms are compared with, start with, are assigned or have appended, and
/// the status class holds every declared place. An input may be tied to a
/// literal indirectly, through another field, a list or a status, so the
/// inputs share the literals of every class that holds one of them. A literal
/// only compared with terms outside those classes (`status in ['Live']` in a
/// spec that compares no input with its status) is no different from a fresh
/// symbol to them, and choosing it would only add states.
pub(super) fn string_literals(
    automaton: &Automaton,
    inputs: &BTreeSet<String>,
) -> BTreeMap<String, BTreeSet<String>> {
    fn visit(
        expr: &Expr,
        links: &mut Vec<(Term, Term)>,
        held: &mut BTreeMap<Term, BTreeSet<String>>,
    ) {
        let mut hold = |t: Term, literal: &Literal| {
            if let Literal::Str(text) = literal {
                held.entry(t).or_default().insert(text.clone());
            }
        };
        match expr {
            Expr::Not(inner) => visit(inner, links, held),
            Expr::And(parts) | Expr::Or(parts) => {
                parts.iter().for_each(|part| visit(part, links, held));
            }
            Expr::Implies(a, b) => {
                visit(a, links, held);
                visit(b, links, held);
            }
            Expr::Compare { lhs, rhs, .. } => match (term(lhs), term(rhs), lhs, rhs) {
                (Some(a), Some(b), _, _) => links.push((a, b)),
                (Some(t), None, _, Operand::Lit(literal))
                | (None, Some(t), Operand::Lit(literal), _) => hold(t, literal),
                _ => {}
            },
            Expr::In { value, set, .. } => match (term(value), set, value) {
                (Some(t), Set::List(items), _) => {
                    items.iter().for_each(|item| hold(t.clone(), item))
                }
                (Some(t), Set::Var(list), _) => links.push((t, Term::Var(list.clone()))),
                (None, Set::Var(list), Operand::Lit(literal)) => {
                    hold(Term::Var(list.clone()), literal)
                }
                _ => {}
            },
            _ => {}
        }
    }
    let mut links = Vec::new();
    let mut held: BTreeMap<Term, BTreeSet<String>> = BTreeMap::new();
    held.insert(
        Term::Status,
        automaton.automaton.states.iter().cloned().collect(),
    );
    for sv in &automaton.state {
        if let Initial::String(value) = &sv.initial {
            held.entry(Term::Var(sv.name.clone()))
                .or_default()
                .insert(value.clone());
        }
    }
    for action in &automaton.actions {
        visit(&action.guard, &mut links, &mut held);
        for effect in &action.effect {
            let (target, value) = match effect {
                Effect::Assign { var, value, .. } => (var, value),
                Effect::Append { list, value } => (list, value),
                _ => continue,
            };
            match value {
                Arg::Lit(Literal::Str(text)) => {
                    held.entry(Term::Var(target.clone()))
                        .or_default()
                        .insert(text.clone());
                }
                Arg::Var(source) | Arg::Param(source) => {
                    links.push((Term::Var(target.clone()), Term::Var(source.clone())));
                }
                Arg::Lit(_) => {}
            }
        }
        for constraint in &action.constraints {
            if let Some(field) = constraint.field() {
                links.push((
                    Term::Var(constraint.param().to_string()),
                    Term::Var(field.to_string()),
                ));
            }
        }
    }
    for assert in rule_asserts(automaton) {
        visit(assert, &mut links, &mut held);
    }
    let mut reached: BTreeSet<Term> = inputs.iter().cloned().map(Term::Var).collect();
    loop {
        let before = reached.len();
        for (a, b) in &links {
            if reached.contains(a) || reached.contains(b) {
                reached.insert(a.clone());
                reached.insert(b.clone());
            }
        }
        if reached.len() == before {
            break;
        }
    }
    let literals: BTreeSet<String> = reached
        .iter()
        .filter_map(|t| held.get(t))
        .flatten()
        .cloned()
        .collect();
    inputs
        .iter()
        .map(|var| (var.clone(), literals.clone()))
        .collect()
}
