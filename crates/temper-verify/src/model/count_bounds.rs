//! How far each count parameter of a move is explored: as far as the move's
//! own arithmetic keeps every counter the parameter lands in inside its
//! bound.
//!
//! Each counter's value at the end of the move is written as the largest of
//! a few linear forms over the move's count parameters and the counters'
//! values before it. A subtraction stops at 0, which adds the form 0, so a
//! counter ends inside its bound exactly when each of its forms does. Those
//! limits give each parameter the largest value it can take.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{Arg, Literal, ParamKind};

use super::semantics::{counter_bound, list_bound};
use super::types::{ModelEffect, ResolvedTransition, TemperModel};

/// The values one count parameter of a move is explored over: `0..=largest`.
pub(super) struct CountRange {
    /// The largest value tried.
    pub(super) largest: usize,
    /// The counters, each with its bound, that a value past `largest` can
    /// take past that bound. Such a step is left out, so a pass over the
    /// move is bounded. Empty when every larger value ends the move as one
    /// tried does.
    pub(super) past: BTreeMap<String, usize>,
    /// Whether the range rests on a value the move's arithmetic does not
    /// settle: another parameter taken as 0 because nothing limits it, or a
    /// counter whose value could not be worked out. A pass over the move is
    /// then bounded.
    pub(super) widened: bool,
}

/// What one term of a linear form stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Term<'a> {
    /// A count parameter of the move.
    Param(&'a str),
    /// A counter's value before the move.
    Before(&'a str),
}

/// The terms of a linear form, each with its whole-number coefficient.
type Terms<'a> = BTreeMap<Term<'a>, i64>;

/// A counter's value partway through the move.
#[derive(Clone, Debug)]
enum Value<'a> {
    /// The largest of these forms, each its terms plus the constant stored
    /// with them.
    Largest(BTreeMap<Terms<'a>, i64>),
    /// A value that could not be worked out, with the count parameters it
    /// depends on.
    Unknown(BTreeSet<&'a str>),
}

impl<'a> Value<'a> {
    fn constant(n: i64) -> Self {
        Value::Largest(BTreeMap::from([(Terms::new(), n)]))
    }

    fn term(term: Term<'a>) -> Self {
        Value::Largest(BTreeMap::from([(Terms::from([(term, 1)]), 0)]))
    }

    fn params(&self) -> BTreeSet<&'a str> {
        match self {
            Value::Largest(forms) => forms
                .keys()
                .flat_map(|terms| terms.keys())
                .filter_map(|term| match term {
                    Term::Param(p) => Some(*p),
                    Term::Before(_) => None,
                })
                .collect(),
            Value::Unknown(params) => params.clone(),
        }
    }

    /// `self + sign * other`: each form of one plus `sign` times each form
    /// of the other, since the larger of sums is the sum of the larger.
    fn combine(&self, other: &Value<'a>, sign: i64) -> Value<'a> {
        let (Value::Largest(ours), Value::Largest(theirs)) = (self, other) else {
            return Value::Unknown(&self.params() | &other.params());
        };
        let mut sums = BTreeMap::new();
        for (our_terms, our_constant) in ours {
            for (their_terms, their_constant) in theirs {
                let mut terms = our_terms.clone();
                for (term, coefficient) in their_terms {
                    let entry = terms.entry(*term).or_insert(0);
                    *entry = entry.saturating_add(coefficient.saturating_mul(sign));
                }
                terms.retain(|_, coefficient| *coefficient != 0);
                let constant = our_constant.saturating_add(their_constant.saturating_mul(sign));
                // Of two forms with the same terms, the larger constant wins.
                let kept = sums.entry(terms).or_insert(i64::MIN);
                *kept = (*kept).max(constant);
            }
        }
        Value::Largest(sums)
    }

    /// `self - other`, stopping at 0 as L1 and the runtime do.
    fn take(&self, other: &Value<'a>) -> Value<'a> {
        match other {
            Value::Largest(forms) if forms.len() == 1 => match self.combine(other, -1) {
                Value::Largest(mut left) => {
                    let zero = left.entry(Terms::new()).or_insert(0);
                    *zero = (*zero).max(0);
                    Value::Largest(left)
                }
                unknown @ Value::Unknown(_) => unknown,
            },
            // Taking the larger of several values leaves the smaller of
            // several, which is not the larger of any forms.
            _ => Value::Unknown(&self.params() | &other.params()),
        }
    }
}

/// One limit a counter's bound puts on the move: `terms + constant <= bound`.
struct Limit<'a> {
    counter: &'a str,
    terms: Terms<'a>,
    constant: i64,
    bound: i64,
}

impl Limit<'_> {
    fn coefficient(&self, param: &str) -> i64 {
        self.terms
            .iter()
            .find_map(|(term, c)| matches!(term, Term::Param(p) if *p == param).then_some(*c))
            .unwrap_or(0)
    }
}

/// What the move's effects do with its count parameters.
struct Walked<'a> {
    /// The limits the bounds of the counters it writes put on it.
    limits: Vec<Limit<'a>>,
    /// Parameters that reach a counter whose value could not be worked out,
    /// and so lost the limit that counter's bound puts on them.
    unsure: BTreeSet<&'a str>,
    /// Parameters used as a list position, with the largest position worth
    /// trying: the first one past the end, which leaves the list as it is.
    positions: BTreeMap<&'a str, usize>,
}

fn walk<'a>(model: &TemperModel, t: &'a ResolvedTransition) -> Walked<'a> {
    let mut values: BTreeMap<&str, Value> = BTreeMap::new();
    let mut positions: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, effect) in t.effects.iter().enumerate() {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = arg_value(&values, value);
                values.insert(var, value);
            }
            ModelEffect::AddCounter { var, value } => {
                let sum = value_of(&values, var).combine(&arg_value(&values, value), 1);
                values.insert(var, sum);
            }
            ModelEffect::SubCounter { var, value } => {
                let left = value_of(&values, var).take(&arg_value(&values, value));
                values.insert(var, left);
            }
            ModelEffect::ListRemoveAt { var, index } => {
                if let Arg::Param(p) = index {
                    let appended = t.effects[..i]
                        .iter()
                        .filter(|earlier| {
                            matches!(earlier, ModelEffect::ListAppend { var: v, .. } if v == var)
                        })
                        .count();
                    let worth = list_bound(model, var).saturating_add(appended);
                    let entry = positions.entry(p.as_str()).or_insert(0);
                    *entry = (*entry).max(worth);
                }
            }
            ModelEffect::SetBool { .. } | ModelEffect::ListAppend { .. } => {}
        }
    }
    let mut limits = Vec::new();
    let mut unsure = BTreeSet::new();
    for (counter, value) in values {
        match value {
            Value::Largest(forms) => {
                let bound = whole(counter_bound(model, counter));
                limits.extend(
                    forms
                        .into_iter()
                        .filter(|(terms, _)| terms.keys().any(|t| matches!(t, Term::Param(_))))
                        .map(|(terms, constant)| Limit {
                            counter,
                            terms,
                            constant,
                            bound,
                        }),
                );
            }
            Value::Unknown(params) => unsure.extend(params),
        }
    }
    Walked {
        limits,
        unsure,
        positions,
    }
}

/// `var`'s value at this point of the move: what an earlier effect wrote,
/// or its value before the move.
fn value_of<'a>(values: &BTreeMap<&'a str, Value<'a>>, var: &'a str) -> Value<'a> {
    values
        .get(var)
        .cloned()
        .unwrap_or_else(|| Value::term(Term::Before(var)))
}

fn arg_value<'a>(values: &BTreeMap<&'a str, Value<'a>>, arg: &'a Arg) -> Value<'a> {
    match arg {
        // A negative amount counts as none, as in L1.
        Arg::Lit(Literal::Int(n)) => Value::constant((*n).max(0)),
        Arg::Param(p) => Value::term(Term::Param(p)),
        Arg::Var(source) => value_of(values, source),
        Arg::Lit(_) => Value::constant(0),
    }
}

/// The values worth trying for each count parameter of `t`.
///
/// A parameter goes as far as every counter it lands in stays inside its
/// bound. Where that depends on another parameter that nothing limits, as
/// `a` does in `x = a - b`, the other is taken as 0 and the range is
/// widened. A parameter that only takes from counters goes as far as it
/// still changes how the move ends: until every form it lowers is at 0.
pub(super) fn count_ranges<'a>(
    model: &TemperModel,
    t: &'a ResolvedTransition,
) -> BTreeMap<&'a str, CountRange> {
    let walked = walk(model, t);
    let counts: Vec<&str> = t
        .params
        .iter()
        .filter(|(_, kind)| matches!(kind, ParamKind::Count))
        .map(|(name, _)| name.as_str())
        .collect();
    let mut most: BTreeMap<&str, Option<i64>> = counts.iter().map(|p| (*p, None)).collect();
    tighten(model, &walked.limits, &mut most, None);
    let guessed: BTreeSet<&str> = counts
        .iter()
        .copied()
        .filter(|p| most[p].is_none() && walked.limits.iter().any(|l| l.coefficient(p) > 0))
        .collect();
    tighten(model, &walked.limits, &mut most, Some(&guessed));

    let widest = widest(model);
    counts
        .iter()
        .map(|p| {
            let range = match most[p] {
                Some(limit) => CountRange {
                    largest: usize::try_from(limit).unwrap_or(0),
                    past: walked
                        .limits
                        .iter()
                        .filter(|l| l.coefficient(p) > 0)
                        .map(|l| (l.counter.to_string(), counter_bound(model, l.counter)))
                        .collect(),
                    widened: guessed.contains(p),
                },
                None => {
                    let worth = lowering_worth(model, &walked.limits, &most, p)
                        .max(walked.positions.get(p).copied());
                    let unsure = walked.unsure.contains(p);
                    let largest = worth.unwrap_or(model.default_max_counter);
                    CountRange {
                        largest: if unsure { largest.max(widest) } else { largest },
                        past: BTreeMap::new(),
                        widened: unsure,
                    }
                }
            };
            (*p, range)
        })
        .collect()
}

/// Lower each parameter's largest value to what every limit allows, given
/// the others' largest values. A parameter nothing limits yet stops a limit
/// that subtracts it from saying anything, unless `guessing` names the
/// parameters to update taking it as 0. Each pass only lowers values, and
/// stopping early leaves them higher, which explores more, never less.
fn tighten<'a>(
    model: &TemperModel,
    limits: &[Limit<'a>],
    most: &mut BTreeMap<&'a str, Option<i64>>,
    guessing: Option<&BTreeSet<&'a str>>,
) {
    for _ in 0..=most.len() {
        let mut lowered = false;
        for limit in limits {
            for (term, coefficient) in &limit.terms {
                let Term::Param(p) = term else { continue };
                if *coefficient <= 0 || guessing.is_some_and(|g| !g.contains(p)) {
                    continue;
                }
                let Some(cap) = cap(model, limit, p, *coefficient, most, guessing.is_some()) else {
                    continue;
                };
                if most[p].is_none_or(|m| cap < m) {
                    most.insert(*p, Some(cap));
                    lowered = true;
                }
            }
        }
        if !lowered {
            return;
        }
    }
}

/// The largest value of `p` that keeps `limit` with every other term as
/// small as it can be: 0 where it adds, its largest where it subtracts.
fn cap(
    model: &TemperModel,
    limit: &Limit<'_>,
    p: &str,
    coefficient: i64,
    most: &BTreeMap<&str, Option<i64>>,
    unknown_as_zero: bool,
) -> Option<i64> {
    let mut rest = limit.constant;
    for (term, c) in &limit.terms {
        if *c >= 0 || matches!(term, Term::Param(q) if *q == p) {
            continue;
        }
        let largest = match term {
            Term::Before(var) => whole(counter_bound(model, var)),
            Term::Param(q) => match most.get(q).copied().flatten() {
                Some(m) => m,
                None if unknown_as_zero => 0,
                None => return None,
            },
        };
        rest = rest.saturating_add(c.saturating_mul(largest));
    }
    Some(
        limit
            .bound
            .saturating_sub(rest)
            .div_euclid(coefficient)
            .max(0),
    )
}

/// How far `p`, which only lowers the forms it is in, still changes one:
/// the value that takes the largest of them to 0. `None` when it is in no
/// form.
fn lowering_worth(
    model: &TemperModel,
    limits: &[Limit<'_>],
    most: &BTreeMap<&str, Option<i64>>,
    p: &str,
) -> Option<usize> {
    limits
        .iter()
        .filter(|l| l.coefficient(p) < 0)
        .map(|l| {
            let highest =
                l.terms
                    .iter()
                    .filter(|(_, c)| **c > 0)
                    .fold(l.constant, |sum, (term, c)| {
                        let largest = match term {
                            Term::Before(var) => whole(counter_bound(model, var)),
                            Term::Param(q) => most.get(q).copied().flatten().unwrap_or(0),
                        };
                        sum.saturating_add(c.saturating_mul(largest))
                    });
            let per = l.coefficient(p).saturating_neg();
            let steps = highest.max(0).div_euclid(per) + i64::from(highest.max(0) % per != 0);
            usize::try_from(steps).unwrap_or(0)
        })
        .max()
}

fn whole(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The largest bound of any counter or list: how far a parameter is
/// explored where a counter it reaches could not be worked out.
fn widest(model: &TemperModel) -> usize {
    model
        .counter_bounds
        .values()
        .copied()
        .chain(model.list_bounds.values().copied())
        .fold(model.default_max_counter, usize::max)
}
