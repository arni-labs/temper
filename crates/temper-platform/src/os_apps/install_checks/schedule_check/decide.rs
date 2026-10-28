//! Whether one record state meets a condition, decided by walking the
//! record states that tell its readable comparisons apart.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_spec::predicate::{CmpOp, Env, Expr, Literal, Operand, Set, Truth, Val, eval};

use super::reads;

/// Record states one pair's walk may visit. Anyone who may install an app can
/// send guards over many compared names, and each name multiplies the states
/// by its distinct values, so an unbounded walk could hold an install for
/// minutes. A pair past it is reported undecided, never refused.
const CASE_BUDGET: usize = 100_000;

/// Whether one record state meets a condition.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Joint {
    /// Some state meets it, or the check does not try to prove otherwise.
    CanBothHold,
    /// No state meets it: proved.
    Exclusive,
    /// The check cannot tell, for the reason given.
    Undecided(String),
}

/// Decide `condition` over every record state: its readable comparisons are
/// walked over values that tell them apart, and the rest stay unknown.
/// `unread` names the terms the caller already made [`unknown`].
pub(super) fn decide(condition: &Expr, states: &[String], mut unread: BTreeSet<String>) -> Joint {
    let condition = readable(condition, &mut unread);
    let mut compared: BTreeMap<String, Vec<Literal>> = BTreeMap::new();
    literals(&condition, &mut compared);
    let mut subjects: Vec<(Option<String>, Vec<Value>)> = Vec::new();
    if reads(&condition).contains(&None) {
        let statuses = states.iter().cloned().map(Value::String).collect();
        subjects.push((None, statuses));
    }
    for (name, literals) in compared {
        subjects.push((Some(name), values_for(&literals)));
    }
    let mut budget = CASE_BUDGET;
    match walk(&condition, &subjects, &mut Case::default(), &mut budget) {
        Walk::Holds => Joint::CanBothHold,
        Walk::Fails => Joint::Exclusive,
        Walk::Depends => Joint::Undecided(format!(
            "it turns on {}, which the check does not read",
            unread.into_iter().collect::<Vec<_>>().join(", ")
        )),
        Walk::OutOfBudget => Joint::Undecided(format!(
            "their guards tell more record states apart than the check walks ({CASE_BUDGET})"
        )),
    }
}

/// `condition` with each term the walk cannot read made unknown, and those
/// terms in `unread`.
fn readable(condition: &Expr, unread: &mut BTreeSet<String>) -> Expr {
    let plain =
        |operand: &Operand| matches!(operand, Operand::Status | Operand::Var(_) | Operand::Lit(_));
    let lit = |operand: &Operand| matches!(operand, Operand::Lit(_));
    match condition {
        Expr::Not(inner) => Expr::Not(Box::new(readable(inner, unread))),
        Expr::And(parts) => Expr::And(parts.iter().map(|p| readable(p, unread)).collect()),
        Expr::Or(parts) => Expr::Or(parts.iter().map(|p| readable(p, unread)).collect()),
        Expr::Implies(when, then) => Expr::Implies(
            Box::new(readable(when, unread)),
            Box::new(readable(then, unread)),
        ),
        Expr::Compare { lhs, rhs, .. } if plain(lhs) && plain(rhs) && (lit(lhs) || lit(rhs)) => {
            condition.clone()
        }
        Expr::In {
            value,
            set: Set::List(_),
            ..
        } if plain(value) => condition.clone(),
        Expr::Const(_) | Expr::Empty(_) | Expr::Var(_) => condition.clone(),
        _ if *condition == unknown() => condition.clone(),
        unreadable => {
            unread.insert(unreadable.to_string());
            unknown()
        }
    }
}

/// A term the walk never knows: a parameter with no name, which no spec can
/// write, and a walk reads every parameter as unknown (`Env::param`).
pub(super) fn unknown() -> Expr {
    Expr::Compare {
        lhs: Operand::Param(String::new()),
        op: CmpOp::Eq,
        rhs: Operand::Lit(Literal::Null),
    }
}

/// Each field `condition` reads, with the constants it is compared with.
fn literals(condition: &Expr, into: &mut BTreeMap<String, Vec<Literal>>) {
    match condition {
        Expr::Not(inner) => literals(inner, into),
        Expr::And(parts) | Expr::Or(parts) => {
            for part in parts {
                literals(part, into);
            }
        }
        Expr::Implies(when, then) => {
            literals(when, into);
            literals(then, into);
        }
        Expr::Compare { lhs, rhs, .. } => {
            for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                if let Operand::Var(name) = side {
                    let entry = into.entry(name.clone()).or_default();
                    if let Operand::Lit(literal) = other {
                        entry.push(literal.clone());
                    }
                }
            }
        }
        Expr::In {
            value: Operand::Var(name),
            set: Set::List(items),
            ..
        } => into
            .entry(name.clone())
            .or_default()
            .extend(items.iter().cloned()),
        Expr::Empty(name) | Expr::Var(name) => {
            into.entry(name.clone()).or_default();
        }
        _ => {}
    }
}

/// Values of a field that between them meet every combination its
/// comparisons with `literals` can: absent, each boolean, empty, each
/// constant, a string equal to none, and a number on each side of and between
/// the numeric constants.
fn values_for(literals: &[Literal]) -> Vec<Value> {
    let mut values = vec![
        Value::Null,
        Value::Bool(true),
        Value::Bool(false),
        Value::String(String::new()),
        Value::Array(Vec::new()),
    ];
    let mut numbers: Vec<f64> = Vec::new();
    // Longer than every string constant, so equal to none of them.
    let mut other = String::from("~");
    for literal in literals {
        match literal {
            Literal::Str(text) => {
                other.push_str(text);
                values.push(Value::String(text.clone()));
            }
            Literal::Int(number) => numbers.push(*number as f64),
            Literal::Bool(_) | Literal::Null => {}
        }
    }
    values.push(Value::String(other));
    numbers.sort_by(f64::total_cmp);
    numbers.dedup();
    if let (Some(low), Some(high)) = (numbers.first(), numbers.last()) {
        let mut points = vec![low - 1.0, high + 1.0];
        for pair in numbers.windows(2) {
            points.push((pair[0] + pair[1]) / 2.0);
        }
        points.extend(&numbers);
        values.extend(points.into_iter().map(Value::from));
    }
    let mut distinct: Vec<Value> = Vec::with_capacity(values.len());
    for value in values {
        if !distinct.contains(&value) {
            distinct.push(value);
        }
    }
    distinct
}

/// The status and fields a walk has fixed so far; the rest read as unknown.
#[derive(Default)]
struct Case<'a> {
    status: Option<&'a str>,
    vars: BTreeMap<&'a str, &'a Value>,
}

impl Env for Case<'_> {
    fn status(&self) -> Val<'_> {
        self.status.map_or(Val::Unknown, Val::Str)
    }

    fn var(&self, name: &str) -> Val<'_> {
        self.vars
            .get(name)
            .map_or(Val::Unknown, |value| Val::json(value))
    }

    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}

/// What a walk of the record states below a case found.
enum Walk {
    /// Some state meets the condition, whatever the unread terms are.
    Holds,
    /// No state meets it.
    Fails,
    /// No state surely meets it, and some state may, by the unread terms.
    Depends,
    /// The walk ran past [`CASE_BUDGET`].
    OutOfBudget,
}

/// Walk the values of each subject in turn, pruning a case the three-valued
/// evaluator already decides with the remaining subjects unknown.
fn walk<'a>(
    condition: &Expr,
    subjects: &'a [(Option<String>, Vec<Value>)],
    case: &mut Case<'a>,
    budget: &mut usize,
) -> Walk {
    let Some(left) = budget.checked_sub(1) else {
        return Walk::OutOfBudget;
    };
    *budget = left;
    match eval(condition, &*case) {
        Truth::True => return Walk::Holds,
        Truth::False => return Walk::Fails,
        Truth::Unknown => {}
    }
    let Some(((name, values), rest)) = subjects.split_first() else {
        return Walk::Depends;
    };
    let mut found = Walk::Fails;
    for value in values {
        match name {
            None => case.status = value.as_str(),
            Some(name) => {
                case.vars.insert(name.as_str(), value);
            }
        }
        match walk(condition, rest, case, budget) {
            Walk::Fails => {}
            Walk::Depends => found = Walk::Depends,
            stop @ (Walk::Holds | Walk::OutOfBudget) => {
                found = stop;
                break;
            }
        }
    }
    match name {
        None => case.status = None,
        Some(name) => {
            case.vars.remove(name.as_str());
        }
    }
    found
}
