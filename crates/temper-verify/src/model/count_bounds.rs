//! How far each count parameter of a move is explored, worked backwards
//! through the move's effects from the bounds every counter must end inside.

use std::collections::BTreeMap;

use temper_spec::predicate::{Arg, Literal};

use super::semantics::counter_bound;
use super::types::{ModelEffect, ResolvedTransition, TemperModel};

/// The values one count parameter of a move is explored over: `0..=largest`.
pub(super) struct CountRange {
    /// The largest value tried.
    pub(super) largest: usize,
    /// Whether a larger value might still end the move inside every bound,
    /// in a state no value up to `largest` reaches. The move subtracted a
    /// value that is not a literal from a counter it had written, which
    /// cannot be worked back exactly, so the range is widened instead and a
    /// pass over the move is bounded.
    pub(super) widened: bool,
}

/// The most a counter may hold at one point of a move for the move to end
/// with every counter inside its bound.
#[derive(Clone, Copy)]
struct Room {
    /// `None` when the move overwrites the counter before reading it again,
    /// so any value fits.
    most: Option<usize>,
    /// Whether `most` was widened past what the effects after it need.
    widened: bool,
}

impl Room {
    const ANY: Room = Room {
        most: None,
        widened: false,
    };

    fn up_to(most: usize) -> Room {
        Room {
            most: Some(most),
            widened: false,
        }
    }

    /// The room for a value that must fit both.
    fn and(self, other: Room) -> Room {
        let most = match (self.most, other.most) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        Room {
            most,
            widened: self.widened || other.widened,
        }
    }

    fn plus(self, n: usize) -> Room {
        Room {
            most: self.most.map(|most| most.saturating_add(n)),
            ..self
        }
    }

    fn minus(self, n: usize) -> Room {
        Room {
            most: self.most.map(|most| most.saturating_sub(n)),
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
/// The parameter's limit is the least room of any counter it lands in.
/// Where larger values all act alike (a subtraction that empties a counter,
/// a position past a list's end) it reaches only as far as that.
pub(super) fn count_range(model: &TemperModel, t: &ResolvedTransition, name: &str) -> CountRange {
    let widest = widest(model);
    let mut room: BTreeMap<&str, Room> = BTreeMap::new();
    let room_of = |room: &BTreeMap<&str, Room>, var: &str| {
        room.get(var)
            .copied()
            .unwrap_or_else(|| Room::up_to(counter_bound(model, var)))
    };
    let mut limit: Option<usize> = None;
    let mut reach: Option<usize> = None;
    let mut widened = false;
    let mut takes = |most: usize, flagged: bool, bound: &mut Option<usize>, least: bool| {
        *bound = Some(match *bound {
            Some(b) if least => b.min(most),
            Some(b) => b.max(most),
            None => most,
        });
        widened |= flagged;
    };
    for (i, effect) in t.effects.iter().enumerate().rev() {
        match effect {
            ModelEffect::SetCounter { var, value } | ModelEffect::AddCounter { var, value } => {
                let after = room_of(&room, var);
                let before = match (effect, value) {
                    (ModelEffect::SetCounter { .. }, _) => Room::ANY,
                    (_, Arg::Lit(Literal::Int(n))) => after.minus(literal(*n)),
                    _ => after,
                };
                room.insert(var, before);
                let Some(most) = after.most else { continue };
                match value {
                    Arg::Param(p) if p == name => takes(most, after.widened, &mut limit, true),
                    Arg::Var(source) => {
                        let fits = room_of(&room, source).and(after);
                        room.insert(source, fits);
                    }
                    _ => {}
                }
            }
            ModelEffect::SubCounter { var, value } => {
                let after = room_of(&room, var);
                if let Arg::Lit(Literal::Int(n)) = value {
                    room.insert(var, after.plus(literal(*n)));
                    continue;
                }
                if after.most.is_none() {
                    continue;
                }
                // What the counter holds before this subtraction, and so the
                // most worth taking from it: its bound, unless the move wrote
                // it, when it depends on values not worked out here.
                let written = t.effects[..i]
                    .iter()
                    .any(|earlier| writes(earlier) == Some(var.as_str()));
                let held = if written {
                    Room {
                        most: Some(widest),
                        widened: true,
                    }
                } else {
                    Room::up_to(counter_bound(model, var))
                };
                let most = held.most.unwrap_or(widest);
                room.insert(
                    var,
                    Room {
                        widened: after.widened || held.widened,
                        ..after.plus(most)
                    },
                );
                match value {
                    Arg::Param(p) if p == name => takes(most, held.widened, &mut reach, false),
                    Arg::Var(source) => {
                        let fits = room_of(&room, source).and(held);
                        room.insert(source, fits);
                    }
                    _ => {}
                }
            }
            ModelEffect::ListRemoveAt { var, index } => {
                // Positions past the end, the first of them included, leave
                // the list as it is.
                let appended = t.effects[..i]
                    .iter()
                    .filter(|earlier| matches!(earlier, ModelEffect::ListAppend { var: v, .. } if v == var))
                    .count();
                let length = list_bound(model, var).saturating_add(appended);
                match index {
                    Arg::Param(p) if p == name => takes(length, false, &mut reach, false),
                    Arg::Var(source) => {
                        let fits = room_of(&room, source).and(Room::up_to(length));
                        room.insert(source, fits);
                    }
                    _ => {}
                }
            }
            ModelEffect::SetBool { .. } | ModelEffect::ListAppend { .. } => {}
        }
    }
    let largest = match (limit, reach) {
        // One past the limit is tried as well: it takes a counter past its
        // bound, so the step is left out and noted, and a pass is bounded.
        (Some(most), _) => most.saturating_add(1),
        (None, Some(most)) => most,
        (None, None) => model.default_max_counter,
    };
    CountRange {
        largest: if widened {
            largest.max(widest)
        } else {
            largest
        },
        widened,
    }
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
        .chain(model.initial_lists.keys().map(|var| list_bound(model, var)))
        .fold(model.default_max_counter, usize::max)
}

fn list_bound(model: &TemperModel, var: &str) -> usize {
    // Removals can be no-ops, so retain a conservative length through them.
    model
        .default_max_counter
        .max(model.initial_lists.get(var).map_or(0, Vec::len))
}
