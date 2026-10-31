//! How far each count parameter of a move is explored, worked backwards
//! through the move's effects from the bounds every counter must end inside.

use std::collections::BTreeMap;

use temper_spec::predicate::{Arg, Literal};

use super::semantics::{counter_bound, list_bound};
use super::types::{ModelEffect, ResolvedTransition, TemperModel};

/// The values one count parameter of a move is explored over: `0..=largest`.
pub(super) struct CountRange {
    /// The largest value tried.
    pub(super) largest: usize,
    /// The counter, with its bound, that a value past `largest` takes past
    /// that bound. Such a step is left out, so a pass over the move is
    /// bounded; `None` when every larger value acts like one tried.
    pub(super) past: Option<(String, usize)>,
    /// Whether the move subtracted a value that is not a literal from a
    /// counter it had written. That cannot be worked back exactly, so the
    /// range is widened to the widest bound, and a pass over the move is
    /// bounded.
    pub(super) widened: bool,
}

/// What a counter may hold at one point of a move for the move to end with
/// every counter inside its bound.
#[derive(Clone, Copy, Default)]
struct Room<'a> {
    /// The most it may hold, and the counter whose bound a larger value
    /// would end past. `None` when no value takes a counter past its bound.
    most: Option<(usize, &'a str)>,
    /// The most worth holding where every larger value ends the move the
    /// same way: a subtraction that empties a counter, a position past a
    /// list's end.
    useful: Option<usize>,
    /// Whether a value here was widened past what the effects after it need.
    widened: bool,
}

impl<'a> Room<'a> {
    fn bound_of(model: &TemperModel, var: &'a str) -> Room<'a> {
        Room {
            most: Some((counter_bound(model, var), var)),
            ..Room::default()
        }
    }

    fn useful_to(most: usize, widened: bool) -> Room<'a> {
        Room {
            useful: Some(most),
            widened,
            ..Room::default()
        }
    }

    /// The room for a value that must serve both.
    fn and(self, other: Room<'a>) -> Room<'a> {
        let most = match (self.most, other.most) {
            (Some(a), Some(b)) => Some(if b.0 < a.0 { b } else { a }),
            (a, b) => a.or(b),
        };
        let useful = match (self.useful, other.useful) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        Room {
            most,
            useful,
            widened: self.widened || other.widened,
        }
    }

    fn shifted(self, by: impl Fn(usize) -> usize) -> Room<'a> {
        Room {
            most: self.most.map(|(most, edge)| (by(most), edge)),
            useful: self.useful.map(&by),
            ..self
        }
    }
}

/// The values worth trying for count parameter `name` of `t`.
///
/// Every counter starts the move inside its bound and must end it there.
/// Working back from the last effect gives each counter's room at each
/// point: an assignment takes the room of the counter it writes, an
/// addition keeps it, and a subtraction widens it by what it takes away.
/// The parameter's limit is the least room of any counter it lands in, and
/// where larger values all act alike it goes only as far as is useful.
pub(super) fn count_range(model: &TemperModel, t: &ResolvedTransition, name: &str) -> CountRange {
    let widest = widest(model);
    let mut room: BTreeMap<&str, Room> = BTreeMap::new();
    let mut taken = Room::default();
    for (i, effect) in t.effects.iter().enumerate().rev() {
        match effect {
            ModelEffect::SetCounter { var, value } | ModelEffect::AddCounter { var, value } => {
                let after = room_at(&room, model, var);
                let before = match (effect, value) {
                    (ModelEffect::SetCounter { .. }, _) => Room::default(),
                    (_, Arg::Lit(Literal::Int(n))) => {
                        after.shifted(|most| most.saturating_sub(literal(*n)))
                    }
                    _ => after,
                };
                room.insert(var, before);
                match value {
                    Arg::Param(p) if p == name => taken = taken.and(after),
                    Arg::Var(source) => {
                        let fits = room_at(&room, model, source).and(after);
                        room.insert(source, fits);
                    }
                    _ => {}
                }
            }
            ModelEffect::SubCounter { var, value } => {
                let after = room_at(&room, model, var);
                if let Arg::Lit(Literal::Int(n)) = value {
                    room.insert(var, after.shifted(|most| most.saturating_add(literal(*n))));
                    continue;
                }
                if after.most.is_none() && after.useful.is_none() {
                    // The move overwrites the result before reading it.
                    continue;
                }
                // What the counter holds before this subtraction, and so the
                // most worth taking from it: its bound, unless the move wrote
                // it, when that depends on values not worked out here.
                let written = t.effects[..i]
                    .iter()
                    .any(|earlier| writes(earlier) == Some(var.as_str()));
                let held = if written {
                    widest
                } else {
                    counter_bound(model, var)
                };
                let mut before = after.shifted(|most| most.saturating_add(held));
                before.widened |= written;
                room.insert(var, before);
                let worth = Room::useful_to(held, written);
                match value {
                    Arg::Param(p) if p == name => taken = taken.and(worth),
                    Arg::Var(source) => {
                        let fits = room_at(&room, model, source).and(worth);
                        room.insert(source, fits);
                    }
                    _ => {}
                }
            }
            ModelEffect::ListRemoveAt { var, index } => {
                // A position past the end, the first of them included, leaves
                // the list as it is.
                let appended = t.effects[..i]
                    .iter()
                    .filter(|earlier| {
                        matches!(earlier, ModelEffect::ListAppend { var: v, .. } if v == var)
                    })
                    .count();
                let worth = Room::useful_to(list_bound(model, var).saturating_add(appended), false);
                match index {
                    Arg::Param(p) if p == name => taken = taken.and(worth),
                    Arg::Var(source) => {
                        let fits = room_at(&room, model, source).and(worth);
                        room.insert(source, fits);
                    }
                    _ => {}
                }
            }
            ModelEffect::SetBool { .. } | ModelEffect::ListAppend { .. } => {}
        }
    }
    let largest = match (taken.most, taken.useful) {
        (Some((most, _)), _) => most,
        (None, Some(useful)) => useful,
        (None, None) => model.default_max_counter,
    };
    CountRange {
        largest: if taken.widened {
            largest.max(widest)
        } else {
            largest
        },
        past: taken
            .most
            .map(|(_, edge)| (edge.to_string(), counter_bound(model, edge))),
        widened: taken.widened,
    }
}

/// What `var` may hold here: what the effects after this point need, or its
/// own bound when none of them reads it.
fn room_at<'a>(room: &BTreeMap<&'a str, Room<'a>>, model: &TemperModel, var: &'a str) -> Room<'a> {
    room.get(var)
        .copied()
        .unwrap_or_else(|| Room::bound_of(model, var))
}

/// The counter an effect writes.
fn writes(effect: &ModelEffect) -> Option<&str> {
    match effect {
        ModelEffect::SetCounter { var, .. }
        | ModelEffect::AddCounter { var, .. }
        | ModelEffect::SubCounter { var, .. } => Some(var),
        _ => None,
    }
}

/// A non-negative literal amount; a negative one counts as none.
fn literal(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

/// The largest bound of any counter or list: how far a parameter is
/// explored where the effects cannot be worked back.
fn widest(model: &TemperModel) -> usize {
    model
        .counter_bounds
        .values()
        .copied()
        .chain(model.list_bounds.values().copied())
        .fold(model.default_max_counter, usize::max)
}
