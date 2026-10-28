//! Moves an action schedules that are refused every time it runs.
//!
//! A `schedule('M', s)` or `schedule_at('M', field)` effect makes move `M` on
//! the same record later, with no params. Two moves one action schedules to
//! fire at the same moment see the same record state, so when no state meets
//! both their conditions (`from` and `guard`), at least one of them is refused
//! every time the action runs; on a repeating state timer that is a refusal
//! every round. A scheduled move none of whose `from` states can be reached
//! from where the action leaves the record is refused every time too. Either
//! is a provable defect of the spec, not a policy limit, so an install of a
//! new spec version refuses it (`refuse_doomed_schedules`).
//!
//! The check refuses only what it proves. It decides conditions over the
//! record's status and fields compared with constants, joined by `!`, `&&`,
//! `||` and `=>`, by walking every record state that tells their comparisons
//! apart, with the guard evaluator the runtime uses. A term it cannot read (a
//! parameter, a related record, a field compared with another field, a list's
//! length or members) leaves a pair undecided, and so does a walk past
//! `decide::CASE_BUDGET`: those are reported, never refused. Which of two
//! moves due together lands first is not known, so a pair is refused only when
//! neither order lets both be made: the second read in the state the first
//! leaves.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, ActionKind, Automaton};
use temper_spec::predicate::{Arg, AssignOp, Effect, Expr, Literal, Name, Operand, Set};

use decide::{Joint, decide};

mod decide;

/// What the check found in one spec, in words.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Findings {
    /// Scheduled moves refused every time the action that schedules them runs.
    pub refused: Vec<String>,
    /// Moves fired together that the check could not decide, with why.
    pub undecided: Vec<String>,
}

/// Check every action of `automaton` for scheduled moves it makes certain to
/// be refused.
pub(super) fn check(automaton: &Automaton) -> Findings {
    // A move is every rule of its name; an output action never transitions,
    // so it is no rule the runtime makes.
    let mut moves: BTreeMap<&str, Vec<&Action>> = BTreeMap::new();
    for action in &automaton.actions {
        if action.kind != ActionKind::Output {
            moves.entry(action.name.as_str()).or_default().push(action);
        }
    }
    let (mut refused, mut undecided) = (BTreeSet::new(), BTreeSet::new());
    for scheduler in &automaton.actions {
        let mut together: BTreeMap<FireTime<'_>, BTreeSet<&str>> = BTreeMap::new();
        for effect in &scheduler.effect {
            let (time, name) = match effect {
                Effect::Schedule {
                    action,
                    delay_seconds,
                } => (FireTime::After(*delay_seconds), action.as_str()),
                Effect::ScheduleAt { action, field } => (FireTime::At(field), action.as_str()),
                _ => continue,
            };
            // A move the spec does not declare is spec validation's to refuse.
            let Some(rules) = moves.get(name) else {
                continue;
            };
            if let Some(text) = unreachable(automaton, scheduler, name, rules) {
                refused.insert(text);
            }
            together.entry(time).or_default().insert(name);
        }
        for (time, names) in &together {
            let names: Vec<Move<'_>> = names
                .iter()
                .map(|name| Move {
                    name: *name,
                    rules: &moves[*name],
                })
                .collect();
            for (i, first) in names.iter().enumerate() {
                for second in &names[i + 1..] {
                    let pair = Pair {
                        scheduler: &scheduler.name,
                        time: *time,
                        first,
                        second,
                    };
                    match pair.decide(&automaton.automaton.states) {
                        Joint::CanBothHold => {}
                        Joint::Exclusive => {
                            refused.insert(pair.refusal());
                        }
                        Joint::Undecided(why) => {
                            undecided.insert(pair.undecided(&why));
                        }
                    }
                }
            }
        }
    }
    Findings {
        refused: refused.into_iter().collect(),
        undecided: undecided.into_iter().collect(),
    }
}

/// When a scheduled move fires: a fixed delay after the scheduling move, or
/// at the time a field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FireTime<'a> {
    After(u64),
    At(&'a str),
}

impl std::fmt::Display for FireTime<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FireTime::After(seconds) => write!(f, "after {seconds} s"),
            FireTime::At(field) => write!(f, "at the time in {field}"),
        }
    }
}

/// A move as the runtime makes it: by any of its rules whose `from` and
/// guard hold.
struct Move<'a> {
    name: &'a str,
    rules: &'a [&'a Action],
}

impl Move<'_> {
    /// The condition under which some rule of the move may be made.
    fn condition(&self) -> Expr {
        let mut rules: Vec<Expr> = self
            .rules
            .iter()
            .map(|rule| {
                let from = (!rule.from.is_empty()).then(|| Expr::In {
                    value: Operand::Status,
                    set: Set::List(rule.from.iter().cloned().map(Literal::Str).collect()),
                    negated: false,
                });
                Expr::and(from.into_iter().chain([rule.guard.clone()]).collect())
            })
            .collect();
        match rules.len() {
            1 => rules.pop().unwrap_or_else(Expr::always),
            _ => Expr::Or(rules),
        }
    }

    /// The names the move writes, `None` being the record's status, with the
    /// constant each then holds when the move sets one. With several rules,
    /// which one was made is not known, so nothing written is.
    fn writes(&self) -> BTreeMap<Option<&str>, Option<Literal>> {
        let mut writes = BTreeMap::new();
        let known = self.rules.len() == 1;
        for rule in self.rules {
            if let Some(to) = &rule.to {
                writes.insert(None, known.then(|| Literal::Str(to.clone())));
            }
            for effect in &rule.effect {
                let (written, value) = match effect {
                    Effect::Assign {
                        var,
                        op: AssignOp::Set,
                        value: Arg::Lit(value),
                    } => (var, Some(value.clone())),
                    Effect::Assign { var, .. } => (var, None),
                    Effect::Append { list, .. } | Effect::RemoveAt { list, .. } => (list, None),
                    Effect::Spawn {
                        store_id_in: Some(field),
                        ..
                    } => (field, None),
                    _ => continue,
                };
                writes.insert(Some(written.as_str()), value.filter(|_| known));
            }
        }
        writes
    }

    /// `condition` as it reads once this move has been made: a name the move
    /// sets to a constant reads as that constant, and a term over a name it
    /// writes otherwise is unknown, added to `unread`.
    fn after(&self, condition: &Expr, unread: &mut BTreeSet<String>) -> Expr {
        let writes = self.writes();
        if writes.is_empty() {
            return condition.clone();
        }
        After {
            mover: self.name,
            writes: &writes,
            unread,
        }
        .expr(condition)
    }

    /// The move's condition in words, rule by rule.
    fn describe(&self) -> String {
        let rules: Vec<String> = self
            .rules
            .iter()
            .map(|rule| {
                let mut parts = Vec::new();
                if !rule.from.is_empty() {
                    parts.push(format!("from {}", rule.from.join(", ")));
                }
                if !rule.guard.is_always() {
                    parts.push(format!("`{}`", rule.guard));
                }
                if parts.is_empty() {
                    "any state".to_string()
                } else {
                    parts.join(" when ")
                }
            })
            .collect();
        format!("{} needs {}", self.name, rules.join(" or "))
    }
}

/// Rewrites a condition to read the record a move has just left.
struct After<'a, 'w> {
    mover: &'a str,
    writes: &'w BTreeMap<Option<&'a str>, Option<Literal>>,
    unread: &'w mut BTreeSet<String>,
}

impl After<'_, '_> {
    fn expr(&mut self, condition: &Expr) -> Expr {
        let rewritten = match condition {
            Expr::Const(_) => Some(condition.clone()),
            Expr::Not(inner) => Some(Expr::Not(Box::new(self.expr(inner)))),
            Expr::And(parts) => Some(Expr::And(parts.iter().map(|p| self.expr(p)).collect())),
            Expr::Or(parts) => Some(Expr::Or(parts.iter().map(|p| self.expr(p)).collect())),
            Expr::Implies(when, then) => Some(Expr::Implies(
                Box::new(self.expr(when)),
                Box::new(self.expr(then)),
            )),
            Expr::Compare { lhs, op, rhs } => {
                let (lhs, rhs) = (self.operand(lhs), self.operand(rhs));
                lhs.zip(rhs)
                    .map(|(lhs, rhs)| Expr::Compare { lhs, op: *op, rhs })
            }
            Expr::In {
                value,
                set,
                negated,
            } => {
                let set_written = matches!(set, Set::Var(name) if self.written(name).is_some());
                self.operand(value)
                    .filter(|_| !set_written)
                    .map(|value| Expr::In {
                        value,
                        set: set.clone(),
                        negated: *negated,
                    })
            }
            Expr::Empty(name) => match self.written(name) {
                None => Some(condition.clone()),
                Some(value) => value.map(|value| {
                    Expr::Const(match value {
                        Literal::Null => true,
                        Literal::Str(text) => text.is_empty(),
                        Literal::Int(_) | Literal::Bool(_) => false,
                    })
                }),
            },
            Expr::Var(name) => match self.written(name) {
                None => Some(condition.clone()),
                Some(value) => value.map(|value| Expr::Const(value == Literal::Bool(true))),
            },
        };
        rewritten.unwrap_or_else(|| {
            self.unread
                .insert(format!("{condition} once {} is made", self.mover));
            decide::unknown()
        })
    }

    /// What the move left in `name`: `None` when it does not write it, and
    /// `Some(None)` when what it wrote is not known.
    fn written(&self, name: &str) -> Option<Option<Literal>> {
        self.writes.get(&Some(name)).cloned()
    }

    /// `operand` as it reads after the move; `None` when it is unknown.
    fn operand(&self, operand: &Operand) -> Option<Operand> {
        let read = match operand {
            Operand::Status => None,
            Operand::Var(name) => Some(name.as_str()),
            Operand::Len(name)
            | Operand::CrossStatus { id_field: name, .. }
            | Operand::CrossField { id_field: name, .. } => {
                return match self.writes.get(&Some(name.as_str())) {
                    None => Some(operand.clone()),
                    Some(_) => None,
                };
            }
            Operand::Param(_) | Operand::Lit(_) => return Some(operand.clone()),
        };
        match self.writes.get(&read) {
            None => Some(operand.clone()),
            Some(value) => value.clone().map(Operand::Lit),
        }
    }
}

/// The names `condition` reads; `None` is the record's status.
pub(super) fn reads(condition: &Expr) -> BTreeSet<Option<String>> {
    let mut reads = BTreeSet::new();
    condition.for_each_name(&mut |name| match name {
        Name::Status => {
            reads.insert(None);
        }
        Name::Var(var) => {
            reads.insert(Some(var.to_string()));
        }
        _ => {}
    });
    reads
}

/// Two moves one action schedules to fire at the same time.
struct Pair<'a> {
    scheduler: &'a str,
    time: FireTime<'a>,
    first: &'a Move<'a>,
    second: &'a Move<'a>,
}

impl Pair<'_> {
    /// Whether both moves can be made, in either order: the first in some
    /// record state, the second in the state the first leaves. Which of two
    /// fires due together lands first is not known, so only a pair neither
    /// order lets both be made is exclusive.
    fn decide(&self, states: &[String]) -> Joint {
        let (first, second) = (self.first.condition(), self.second.condition());
        let orders = [
            (self.first, &first, &second),
            (self.second, &second, &first),
        ];
        let mut joint = Joint::Exclusive;
        for (made, made_needs, then_needs) in orders {
            let mut unread = BTreeSet::new();
            let then_needs = made.after(then_needs, &mut unread);
            let both = Expr::and(vec![made_needs.clone(), then_needs]);
            match decide(&both, states, unread) {
                Joint::CanBothHold => return Joint::CanBothHold,
                Joint::Exclusive => {}
                undecided @ Joint::Undecided(_) => joint = undecided,
            }
        }
        joint
    }

    fn together(&self) -> String {
        format!(
            "{} schedules {} and {} to fire together ({})",
            self.scheduler, self.first.name, self.second.name, self.time
        )
    }

    fn refusal(&self) -> String {
        format!(
            "{}, and no record state lets both be made ({}; {}), so one of them is refused \
             every time {} runs",
            self.together(),
            self.first.describe(),
            self.second.describe(),
            self.scheduler
        )
    }

    fn undecided(&self, why: &str) -> String {
        format!(
            "{}, and the check cannot tell whether one record state lets both be made ({}; \
             {}): {why}. If none does, one of them is refused every time {} runs",
            self.together(),
            self.first.describe(),
            self.second.describe(),
            self.scheduler
        )
    }
}

/// Why `scheduled` is refused every time `scheduler` runs, when it is: none
/// of the states its rules may be made from can be reached from the states
/// `scheduler` leaves the record in, by any move of the spec.
fn unreachable(
    automaton: &Automaton,
    scheduler: &Action,
    scheduled: &str,
    rules: &[&Action],
) -> Option<String> {
    let states = &automaton.automaton.states;
    let left: BTreeSet<&str> = match &scheduler.to {
        Some(to) => BTreeSet::from([to.as_str()]),
        None => sources(states, scheduler),
    };
    let mut reached = left.clone();
    let mut frontier: Vec<&str> = left.iter().copied().collect();
    while let Some(state) = frontier.pop() {
        for action in &automaton.actions {
            if let Some(to) = action.to.as_deref()
                && sources(states, action).contains(state)
                && reached.insert(to)
            {
                frontier.push(to);
            }
        }
    }
    let valid: BTreeSet<&str> = rules
        .iter()
        .flat_map(|rule| sources(states, rule))
        .collect();
    if !valid.is_disjoint(&reached) {
        return None;
    }
    let name = &scheduler.name;
    let join = |set: &BTreeSet<&str>| set.iter().copied().collect::<Vec<_>>().join(", ");
    Some(format!(
        "{name} schedules {scheduled}, which can be made only from {}, and no move takes the \
         record there from {} (where {name} leaves it), so {scheduled} is refused every time \
         {name} runs",
        if valid.is_empty() {
            "no state".to_string()
        } else {
            join(&valid)
        },
        join(&left)
    ))
}

/// The states `action` may be made from: its `from`, or every state when it
/// has none (as the runtime's transition table reads it), narrowed by the
/// statuses its guard pins.
fn sources<'a>(states: &'a [String], action: &'a Action) -> BTreeSet<&'a str> {
    let from = if action.from.is_empty() {
        states
    } else {
        action.from.as_slice()
    };
    let pinned = action.guard.required_statuses();
    from.iter()
        .map(String::as_str)
        .filter(|state| pinned.as_ref().is_none_or(|pinned| pinned.contains(state)))
        .collect()
}

#[cfg(test)]
mod tests;
