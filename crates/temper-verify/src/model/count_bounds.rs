//! How far each count parameter of a move is explored: as far as the move's
//! own arithmetic keeps every counter the parameter lands in inside its
//! bound, and every list position it gives no further than one past the end.
//!
//! Each counter's value at the end of the move is written as the largest of
//! a few linear forms over the move's count parameters, each plus a rest
//! that does not depend on them: a constant, or what the counters held
//! before the move, known only to lie between 0 and their bounds. A
//! subtraction stops at 0, which adds the form 0, so a counter ends inside
//! its bound exactly when each of its forms does. Those limits give each
//! parameter the largest value it can take.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{Arg, Literal, ParamKind};

use super::semantics::{MAX_PARAM_ASSIGNMENTS, counter_bound, list_bound};
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

/// The count parameters in a linear form, each with its whole-number
/// coefficient.
type Terms<'a> = BTreeMap<&'a str, i64>;

/// The part of a form that is not parameters: the least and the most it can
/// be, over everything the counters could hold before the move.
#[derive(Clone, Copy, Debug)]
struct Rest {
    least: i64,
    most: i64,
}

/// How many forms one value may hold. Forms come from distinct sums of the
/// move's parameters, and no spec writes as many as a move explores
/// assignments; a contrived chain of additions could, and would fill memory
/// before the assignment budget refuses the move. Past it the value is
/// marked as not worked out, so the move is still explored, widened and
/// reported as bounded.
const MAX_FORMS: usize = MAX_PARAM_ASSIGNMENTS;

/// A counter's value partway through the move.
#[derive(Clone, Debug)]
enum Value<'a> {
    /// The largest of these forms, each its terms plus its rest.
    Largest(BTreeMap<Terms<'a>, Rest>),
    /// A value that could not be worked out, with the count parameters it
    /// depends on.
    Unknown(BTreeSet<&'a str>),
}

impl<'a> Value<'a> {
    fn rest(least: i64, most: i64) -> Self {
        Value::Largest(BTreeMap::from([(Terms::new(), Rest { least, most })]))
    }

    fn param(p: &'a str) -> Self {
        Value::Largest(BTreeMap::from([(
            Terms::from([(p, 1)]),
            Rest { least: 0, most: 0 },
        )]))
    }

    fn params(&self) -> BTreeSet<&'a str> {
        match self {
            Value::Largest(forms) => forms
                .keys()
                .flat_map(|terms| terms.keys().copied())
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
        for (our_terms, ours) in ours {
            for (their_terms, theirs) in theirs {
                let mut terms = our_terms.clone();
                for (p, coefficient) in their_terms {
                    let entry = terms.entry(*p).or_insert(0);
                    *entry = entry.saturating_add(coefficient.saturating_mul(sign));
                }
                terms.retain(|_, coefficient| *coefficient != 0);
                let rest = if sign > 0 {
                    Rest {
                        least: ours.least.saturating_add(theirs.least),
                        most: ours.most.saturating_add(theirs.most),
                    }
                } else {
                    Rest {
                        least: ours.least.saturating_sub(theirs.most),
                        most: ours.most.saturating_sub(theirs.least),
                    }
                };
                keep_largest(&mut sums, terms, rest);
                if sums.len() > MAX_FORMS {
                    return Value::Unknown(&self.params() | &other.params());
                }
            }
        }
        Value::Largest(sums)
    }

    /// `self - other`, stopping at 0 as L1 and the runtime do.
    fn take(&self, other: &Value<'a>) -> Value<'a> {
        match other {
            Value::Largest(forms) if forms.len() == 1 => match self.combine(other, -1) {
                Value::Largest(mut left) => {
                    keep_largest(&mut left, Terms::new(), Rest { least: 0, most: 0 });
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

/// Add a form to `forms`. Two forms with the same terms make one, their
/// larger, whose rest is at least the larger least and at most the larger
/// most.
fn keep_largest<'a>(forms: &mut BTreeMap<Terms<'a>, Rest>, terms: Terms<'a>, rest: Rest) {
    forms
        .entry(terms)
        .and_modify(|kept| {
            kept.least = kept.least.max(rest.least);
            kept.most = kept.most.max(rest.most);
        })
        .or_insert(rest);
}

/// One limit on the move's parameters: `terms + rest <= bound`.
struct Limit<'a> {
    /// The counter whose bound it is, or `None` for a list position, where
    /// `bound` is the first position past the end, which acts as every later
    /// one does.
    counter: Option<&'a str>,
    terms: Terms<'a>,
    rest: Rest,
    bound: i64,
}

impl Limit<'_> {
    fn coefficient(&self, p: &str) -> i64 {
        self.terms.get(p).copied().unwrap_or(0)
    }
}

/// What the move's effects do with its count parameters.
struct Walked<'a> {
    /// The limits the bounds of the counters it writes, and the lists it
    /// removes from, put on them.
    limits: Vec<Limit<'a>>,
    /// Parameters that reach a counter or position whose value could not be
    /// worked out, and so lost the limit it puts on them.
    unsure: BTreeSet<&'a str>,
}

fn walk<'a>(model: &TemperModel, t: &'a ResolvedTransition) -> Walked<'a> {
    let mut values: BTreeMap<&str, Value> = BTreeMap::new();
    let mut limits = Vec::new();
    let mut unsure = BTreeSet::new();
    for (i, effect) in t.effects.iter().enumerate() {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = arg_value(model, &values, value);
                values.insert(var, value);
            }
            ModelEffect::AddCounter { var, value } => {
                let sum =
                    value_of(model, &values, var).combine(&arg_value(model, &values, value), 1);
                values.insert(var, sum);
            }
            ModelEffect::SubCounter { var, value } => {
                let left = value_of(model, &values, var).take(&arg_value(model, &values, value));
                values.insert(var, left);
            }
            ModelEffect::ListRemoveAt { var, index } => {
                let appended = t.effects[..i]
                    .iter()
                    .filter(|earlier| {
                        matches!(earlier, ModelEffect::ListAppend { var: v, .. } if v == var)
                    })
                    .count();
                let past_end = whole(list_bound(model, var).saturating_add(appended));
                match arg_value(model, &values, index) {
                    Value::Largest(forms) => limits.extend(
                        forms
                            .into_iter()
                            .filter(|(terms, _)| !terms.is_empty())
                            .map(|(terms, rest)| Limit {
                                counter: None,
                                terms,
                                rest,
                                bound: past_end,
                            }),
                    ),
                    Value::Unknown(params) => unsure.extend(params),
                }
            }
            ModelEffect::SetBool { .. } | ModelEffect::ListAppend { .. } => {}
        }
    }
    for (counter, value) in values {
        match value {
            Value::Largest(forms) => {
                let bound = whole(counter_bound(model, counter));
                limits.extend(
                    forms
                        .into_iter()
                        .filter(|(terms, _)| !terms.is_empty())
                        .map(|(terms, rest)| Limit {
                            counter: Some(counter),
                            terms,
                            rest,
                            bound,
                        }),
                );
            }
            Value::Unknown(params) => unsure.extend(params),
        }
    }
    Walked { limits, unsure }
}

/// `var`'s value at this point of the move: what an earlier effect wrote,
/// or what it held before the move.
fn value_of<'a>(
    model: &TemperModel,
    values: &BTreeMap<&'a str, Value<'a>>,
    var: &str,
) -> Value<'a> {
    values
        .get(var)
        .cloned()
        .unwrap_or_else(|| Value::rest(0, whole(counter_bound(model, var))))
}

fn arg_value<'a>(
    model: &TemperModel,
    values: &BTreeMap<&'a str, Value<'a>>,
    arg: &'a Arg,
) -> Value<'a> {
    match arg {
        // A negative amount counts as none, as in L1.
        Arg::Lit(Literal::Int(n)) => Value::rest((*n).max(0), (*n).max(0)),
        Arg::Param(p) => Value::param(p),
        Arg::Var(source) => value_of(model, values, source),
        Arg::Lit(_) => Value::rest(0, 0),
    }
}

/// The values worth trying for each count parameter of `t`.
///
/// A parameter goes as far as every counter it lands in stays inside its
/// bound. Where that depends on another parameter that nothing limits, as
/// `a` does in `x = a - b`, the other is taken as 0 and the range is
/// widened. A parameter nothing limits goes as far as it still changes how
/// the move ends: until a position it gives is past the list's end, and
/// until every form it lowers is at 0.
pub(super) fn count_ranges<'a>(
    model: &TemperModel,
    t: &'a ResolvedTransition,
) -> BTreeMap<&'a str, CountRange> {
    let counts: Vec<&str> = t
        .params
        .iter()
        .filter(|(_, kind)| matches!(kind, ParamKind::Count))
        .map(|(name, _)| name.as_str())
        .collect();
    if counts.is_empty() {
        return BTreeMap::new();
    }
    let walked = walk(model, t);
    let bounds: Vec<&Limit> = walked
        .limits
        .iter()
        .filter(|l| l.counter.is_some())
        .collect();
    let mut most: BTreeMap<&str, Option<i64>> = counts.iter().map(|p| (*p, None)).collect();
    tighten(&bounds, &mut most, None);
    let guessed: BTreeSet<&str> = counts
        .iter()
        .copied()
        .filter(|p| most[p].is_none() && bounds.iter().any(|l| l.coefficient(p) > 0))
        .collect();
    tighten(&bounds, &mut most, Some(&guessed));

    let widest = widest(model);
    counts
        .iter()
        .map(|p| {
            let range = match most[p] {
                Some(limit) => CountRange {
                    largest: usize::try_from(limit).unwrap_or(0),
                    past: bounds
                        .iter()
                        .filter(|l| l.coefficient(p) > 0)
                        .filter_map(|l| l.counter)
                        .map(|counter| (counter.to_string(), counter_bound(model, counter)))
                        .collect(),
                    widened: guessed.contains(p),
                },
                None => {
                    let (reach, reach_guessed) = reach(&walked.limits, &most, p);
                    let worth = lowering_worth(&walked.limits, &most, p, widest).max(reach);
                    let unsure = walked.unsure.contains(p);
                    let largest = worth.unwrap_or(model.default_max_counter);
                    CountRange {
                        largest: if unsure { largest.max(widest) } else { largest },
                        past: BTreeMap::new(),
                        widened: unsure || reach_guessed,
                    }
                }
            };
            (*p, range)
        })
        .collect()
}

/// Lower each parameter's largest value to what every bound allows, given
/// the others' largest values. A parameter nothing limits yet stops a bound
/// that subtracts it from saying anything, unless `guessing` names the
/// parameters to update taking it as 0. Each pass only lowers values, and
/// stopping early leaves them higher, which explores more, never less.
fn tighten<'a>(
    bounds: &[&Limit<'a>],
    most: &mut BTreeMap<&'a str, Option<i64>>,
    guessing: Option<&BTreeSet<&'a str>>,
) {
    for _ in 0..=most.len() {
        let mut lowered = false;
        for limit in bounds {
            for (p, coefficient) in &limit.terms {
                if *coefficient <= 0 || guessing.is_some_and(|g| !g.contains(p)) {
                    continue;
                }
                let Some(least) = least_of_others(limit, p, most, guessing.is_some()) else {
                    continue;
                };
                let cap = limit
                    .bound
                    .saturating_sub(least)
                    .div_euclid(*coefficient)
                    .max(0);
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

/// The least the rest of `limit`, besides `p`, can be: its rest's least,
/// every parameter that adds at 0 and every one that subtracts at its
/// largest. `None` when one that subtracts has no largest yet, unless
/// `unknown_as_zero`.
fn least_of_others(
    limit: &Limit<'_>,
    p: &str,
    most: &BTreeMap<&str, Option<i64>>,
    unknown_as_zero: bool,
) -> Option<i64> {
    let mut least = limit.rest.least;
    for (q, c) in &limit.terms {
        if *c >= 0 || *q == p {
            continue;
        }
        let largest = match most.get(q).copied().flatten() {
            Some(m) => m,
            None if unknown_as_zero => 0,
            None => return None,
        };
        least = least.saturating_add(c.saturating_mul(largest));
    }
    Some(least)
}

/// How far `p` goes to give each list position it reaches the first one
/// past the end, whatever the rest is, and whether that took another
/// parameter nothing limits as 0. `None` when it gives no position.
fn reach(
    limits: &[Limit<'_>],
    most: &BTreeMap<&str, Option<i64>>,
    p: &str,
) -> (Option<usize>, bool) {
    let mut guessed = false;
    let reach = limits
        .iter()
        .filter(|l| l.counter.is_none() && l.coefficient(p) > 0)
        .map(|l| {
            let least = least_of_others(l, p, most, false).unwrap_or_else(|| {
                guessed = true;
                least_of_others(l, p, most, true).unwrap_or(l.rest.least)
            });
            let steps = ceil_div(l.bound.saturating_sub(least), l.coefficient(p));
            usize::try_from(steps).unwrap_or(0)
        })
        .max();
    (reach, guessed)
}

/// How far `p` still changes a form it lowers: until the largest that form
/// can be is at 0. A parameter that adds to it counts at its largest, or at
/// `widest` while nothing limits it. `None` when it lowers no form.
fn lowering_worth(
    limits: &[Limit<'_>],
    most: &BTreeMap<&str, Option<i64>>,
    p: &str,
    widest: usize,
) -> Option<usize> {
    limits
        .iter()
        .filter(|l| l.coefficient(p) < 0)
        .map(|l| {
            let highest =
                l.terms
                    .iter()
                    .filter(|(_, c)| **c > 0)
                    .fold(l.rest.most, |sum, (q, c)| {
                        let largest = most.get(q).copied().flatten().unwrap_or(whole(widest));
                        sum.saturating_add(c.saturating_mul(largest))
                    });
            let steps = ceil_div(highest, l.coefficient(p).saturating_neg());
            usize::try_from(steps).unwrap_or(0)
        })
        .max()
}

/// `n / d` rounded up, for `d > 0`, and 0 for `n <= 0`.
fn ceil_div(n: i64, d: i64) -> i64 {
    if n <= 0 {
        0
    } else {
        n.div_euclid(d) + i64::from(n.rem_euclid(d) != 0)
    }
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
