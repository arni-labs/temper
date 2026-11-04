//! What a walk's threads remember of the moves they make.
//!
//! A walk's states differ mostly in what is in flight, and a move does the same
//! wherever the records, timers, values and sequence numbers are the same: in
//! one walk nearly every move has been made before. So each thread keeps what
//! it found by those, as a [`Place`], and what a move changes as a [`Way`].
//! What depends on what is in flight as a whole is worked out for each state
//! ([`JointModel::finished`]); a move that read it some other way says so, and
//! is not kept. Debug builds check what is remembered against the move made
//! afresh.

use std::collections::BTreeSet;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use super::super::super::pack::Packer;
use super::super::super::room::{Heap, block};
use super::super::super::state::canonical_quietly;
use super::super::super::{Call, Dispatch, JointState};
use super::super::order::values;
use super::super::step::{Certainty, Move, Outcome};
use super::super::{Delivery, JointModel};
use crate::model::{Fields, ModelString, TemperModelState};

/// How many steps a thread checks before it starts to skip most of them.
const CHECK_FIRST: u64 = 1 << 12;

/// And then one in this many, so a long walk in a debug build still ends.
const CHECK_EVERY: u64 = 1 << 6;

/// Whether to make this remembered step afresh and compare.
pub(super) fn check_step(steps: u64) -> bool {
    cfg!(feature = "check-every-remembered-step")
        || (cfg!(debug_assertions) && (steps < CHECK_FIRST || steps.is_multiple_of(CHECK_EVERY)))
}

/// A sorted list's change: what left it and what joined it.
struct Change<T> {
    lost: Vec<Arc<T>>,
    gained: Vec<Arc<T>>,
}

impl<T: Ord> Change<T> {
    /// What changes `before` into `after`; both are sorted.
    fn between(before: &[Arc<T>], after: &[Arc<T>]) -> Self {
        let (mut lost, mut gained) = (Vec::new(), Vec::new());
        let (mut was, mut is) = (before.iter().peekable(), after.iter().peekable());
        loop {
            match (was.peek(), is.peek()) {
                (Some(a), Some(b)) => match a.cmp(b) {
                    std::cmp::Ordering::Equal => {
                        was.next();
                        is.next();
                    }
                    std::cmp::Ordering::Less => lost.extend(was.next().cloned()),
                    std::cmp::Ordering::Greater => gained.extend(is.next().cloned()),
                },
                (Some(_), None) => lost.extend(was.next().cloned()),
                (None, Some(_)) => gained.extend(is.next().cloned()),
                (None, None) => return Self { lost, gained },
            }
        }
    }

    /// `list`, sorted, with this change made, written to `out`.
    fn on(&self, list: &[Arc<T>], out: &mut Vec<Arc<T>>) {
        out.clear();
        let mut lost = self.lost.iter().peekable();
        for held in list {
            if lost.peek().is_some_and(|gone| *gone == held) {
                lost.next();
            } else {
                out.push(held.clone());
            }
        }
        for gained in &self.gained {
            let at = out.partition_point(|held| held < gained);
            out.insert(at, gained.clone());
        }
    }

    fn bytes(&self) -> usize {
        (self.lost.len() + self.gained.len()) * size_of::<Arc<T>>()
    }
}

/// One way a move goes from a place: the records and timers it leaves, which
/// only the place decides, and what it changes of what is on its way.
struct Way {
    records: Fields<Arc<TemperModelState>>,
    armed: Fields<u32>,
    inflight: Change<Dispatch>,
    calls: Change<Call>,
    certainty: Certainty,
}

impl Way {
    /// How `outcome` differs from `state`, holding the values the tables hold
    /// where they hold them.
    fn between(state: &JointState, outcome: &Outcome, packer: &Packer) -> Self {
        let (after, _, certainty) = outcome;
        let mut records = after.records.clone();
        for record in records.values_mut() {
            if let Some(held) = packer.held_record(record)
                && !Arc::ptr_eq(&held, record)
            {
                *record = held;
            }
        }
        let mut inflight = Change::between(&state.inflight, &after.inflight);
        for dispatch in &mut inflight.gained {
            if let Some(held) = packer.held_dispatch(dispatch) {
                *dispatch = held;
            }
        }
        Self {
            records,
            armed: after.armed.clone(),
            inflight,
            calls: Change::between(&state.calls, &after.calls),
            certainty: *certainty,
        }
    }

    /// Where this way goes from `state`, written to `out`.
    fn on(&self, state: &JointState, out: &mut JointState) {
        out.records.clone_from(&self.records);
        out.armed.clone_from(&self.armed);
        self.inflight.on(&state.inflight, &mut out.inflight);
        self.calls.on(&state.calls, &mut out.calls);
    }

    /// Bytes it holds that nothing else does: the records that are new to
    /// it, and what it lists.
    fn bytes(&self, state: &JointState) -> usize {
        let new_records = self
            .records
            .values()
            .zip(state.records.values())
            .filter(|(is, was)| !Arc::ptr_eq(is, was))
            .map(|(is, _)| block(size_of::<TemperModelState>()) + is.heap())
            .sum::<usize>();
        let gained = self.inflight.gained.iter().map(|d| d.heap()).sum::<usize>();
        block(size_of::<Self>() + self.inflight.bytes() + self.calls.bytes()) + new_records + gained
    }
}

impl JointModel {
    /// `state`, which a move left, in its one form, unless it would put more
    /// in flight than the walk keeps: as [`Self::finished`] leaves it.
    pub(super) fn settled_quietly(&self, state: &mut JointState) -> bool {
        if self.delivery != Delivery::BeforeNextRequest && !self.fits(state) {
            return false;
        }
        let rewrote = self.order.normalize(state);
        self.in_one_order(state);
        canonical_quietly(state, rewrote);
        true
    }
}

/// A set of moves a place has, and what each does once it has been made.
#[derive(Default)]
pub(super) struct Group {
    filled: bool,
    pub(super) moves: Vec<Move>,
    ways: Vec<Option<Box<[Way]>>>,
}

impl Group {
    /// Whether its moves are known.
    pub(super) fn filled(&self) -> bool {
        self.filled
    }

    /// Hold `moves` as this group's, and say how many bytes that takes.
    pub(super) fn fill(&mut self, moves: Vec<Move>) -> usize {
        self.ways = moves.iter().map(|_| None).collect();
        let bytes = block(moves.capacity() * size_of::<Move>())
            + moves.iter().map(Heap::heap).sum::<usize>()
            + block(self.ways.capacity() * size_of::<Option<Box<[Way]>>>());
        self.moves = moves;
        self.filled = true;
        bytes
    }

    /// Whether move `at` has been made from this place, and kept.
    pub(super) fn remembers(&self, at: usize) -> bool {
        self.ways[at].is_some()
    }

    /// How many ways move `at` goes, as this place remembers it.
    pub(super) fn way_count(&self, at: usize) -> usize {
        self.ways[at].as_ref().map_or(0, |ways| ways.len())
    }

    /// Where way `way` of move `at` goes from `state`, written to `out`, and
    /// what the model knows of it.
    pub(super) fn way(
        &self,
        at: usize,
        way: usize,
        state: &JointState,
        out: &mut JointState,
    ) -> Certainty {
        let way = &self.ways[at].as_ref().expect("it remembers this move")[way];
        way.on(state, out);
        way.certainty
    }

    /// Keep `outcomes` as where move `at` goes from this place, and say how
    /// many bytes that takes.
    pub(super) fn keep(
        &mut self,
        at: usize,
        state: &JointState,
        outcomes: &[Outcome],
        packer: &Packer,
    ) -> usize {
        let ways: Box<[Way]> = outcomes
            .iter()
            .map(|outcome| Way::between(state, outcome, packer))
            .collect();
        let bytes = ways.iter().map(|way| way.bytes(state)).sum();
        self.ways[at] = Some(ways);
        bytes
    }
}

/// What a thread found of one place: the records' moves, and those of each
/// call and dispatch that has been on its way there.
#[derive(Default)]
pub(super) struct Place {
    pub(super) records: Group,
    pub(super) calls: FxHashMap<u32, Group>,
    pub(super) dispatches: FxHashMap<u32, Group>,
}

/// What one thread of a walk remembers.
#[derive(Default)]
pub(super) struct Memo {
    pub(super) places: Places,
    /// Steps it has made from what it remembers.
    pub(super) steps: u64,
    /// Where a state a move leads to is packed as it is looked up.
    pub(super) packed: Vec<u32>,
    /// Where a state a move leads to is written as it is worked out.
    pub(super) scratch: JointState,
}

/// Each place a thread has been, by what names it.
#[derive(Default)]
pub(super) struct Places {
    places: FxHashMap<Box<[u64]>, Place>,
    /// Where a place's name is written as it is looked up.
    name: Vec<u64>,
}

impl Places {
    /// The place `state` is at, and the bytes it took if it is new. What a
    /// move reads of a state is what names it: `held` and `armed`, the
    /// numbers of its records and timers; the values it holds, which the
    /// strings a move chooses and makes up come from; and each family's
    /// sequence numbers, which say whether a count stays exact.
    pub(super) fn place(
        &mut self,
        model: &JointModel,
        (held, armed): (&[u32], &[u32]),
        state: &JointState,
        strings: &BTreeSet<ModelString>,
    ) -> (&mut Place, usize) {
        let name = &mut self.name;
        name.clear();
        name.extend(held.iter().chain(armed).map(|&word| u64::from(word)));
        name.push(u64::MAX);
        for value in strings {
            let (kind, word) = match value {
                ModelString::Literal(text) => (0, text.address() as u64),
                ModelString::Symbol(id) => (1, *id as u64),
                ModelString::BlankSymbol(id) => (2, *id as u64),
            };
            name.extend([kind, word]);
        }
        for family in &model.order.families {
            name.push(u64::MAX);
            name.extend(values(family, state).into_iter().map(|n| n as u64));
        }
        let mut bytes = 0;
        if !self.places.contains_key(name.as_slice()) {
            bytes = block(name.len() * size_of::<u64>()) + 2 * size_of::<(Box<[u64]>, Place)>();
            self.places.insert(name.as_slice().into(), Place::default());
        }
        let place = self
            .places
            .get_mut(name.as_slice())
            .expect("the place was just found or added");
        (place, bytes)
    }
}
