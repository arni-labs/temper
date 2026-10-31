//! Records that can never reach a place another record in the same place
//! still can.
//!
//! The model check's whole walk keeps its graph. From it, each state gets the
//! places it can still reach. A state loses a place when another state in the
//! same place can reach it and this one never can: a cup sent back to the line
//! that no barista may claim again, or an order that chose delivery and can no
//! longer be picked up. The first is a mistake and the second a choice; the
//! checker can't tell them apart, so each is named for the author to read and
//! nothing fails.
//!
//! A state with a step past a counter's bound somewhere ahead is not named:
//! what lies past the bound was not walked, so what it can reach is not known.
//! A loss is named in the place where a move made it, not again in each place
//! the record passes through after it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::model::{TemperModel, TemperModelState};

/// A record that can never reach places another record in its place can.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LostPlace {
    /// The place both records are in.
    pub place: String,
    /// The places the record can never reach.
    pub lost: Vec<String>,
    /// The record, at the shortest path to where it lost them.
    pub record: TemperModelState,
    /// The moves to it from the start.
    pub path: Vec<String>,
    /// A record in the same place that can still reach some of them.
    pub other: TemperModelState,
    /// The moves to it from the start.
    pub other_path: Vec<String>,
    /// The places in `lost` the other record can reach.
    pub other_reaches: Vec<String>,
}

impl LostPlace {
    /// The finding in plain words: "a record in Waiting with makers = 1 can
    /// never reach BeingMade or Made, while one with makers = 0 can (the
    /// first after ..., the second after ...)".
    pub fn describe(&self) -> String {
        let (mine, theirs) = differences(&self.record, &self.other);
        let reaches = if self.other_reaches == self.lost {
            "can".to_string()
        } else {
            format!("can reach {}", either(&self.other_reaches))
        };
        format!(
            "a record in {} with {} can never reach {}, while one with {} {} (the first {}, the \
             second {})",
            self.place,
            mine,
            either(&self.lost),
            theirs,
            reaches,
            after(&self.path),
            after(&self.other_path),
        )
    }
}

/// The states a whole walk found, each with the place it is in, whether a
/// step from it was left out past a bound, how it was first reached, and the
/// states it steps to. A state's number is the order it was found in, and a
/// state's steps are added when the walk takes it, in that order.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    place: Vec<u32>,
    cut: Vec<bool>,
    parent: Vec<Option<(u32, u32)>>,
    depth: Vec<u32>,
    offsets: Vec<u32>,
    steps: Vec<u32>,
}

/// A state's place when its status is not a declared place.
const NO_PLACE: u32 = u32::MAX;

impl Graph {
    /// Number a newly found state, first reached from `parent` by the
    /// transition at that index, or a start state.
    pub(crate) fn add(
        &mut self,
        model: &TemperModel,
        state: &TemperModelState,
        parent: Option<(u32, u32)>,
    ) -> u32 {
        let at = u32::try_from(self.place.len()).expect("a walk numbers fewer than 2^32 states");
        let place = model.states.iter().position(|s| *s == state.status);
        self.place
            .push(place.map_or(NO_PLACE, |p| u32::try_from(p).expect("places fit in u32")));
        self.cut.push(false);
        self.depth
            .push(parent.map_or(0, |(from, _)| self.depth[from as usize] + 1));
        self.parent.push(parent);
        at
    }

    /// The steps from state `at`, the next state the walk takes, and whether
    /// one was left out past a bound.
    pub(crate) fn steps_from(&mut self, at: u32, cut: bool, mut to: Vec<u32>) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        assert_eq!(
            at as usize,
            self.offsets.len() - 1,
            "the walk takes states in the order it found them"
        );
        to.sort_unstable();
        to.dedup();
        self.cut[at as usize] = cut;
        self.steps.extend(to);
        self.offsets
            .push(u32::try_from(self.steps.len()).expect("a walk has fewer than 2^32 steps"));
    }

    fn len(&self) -> usize {
        self.place.len()
    }

    fn steps(&self, at: usize) -> &[u32] {
        &self.steps[self.offsets[at] as usize..self.offsets[at + 1] as usize]
    }

    fn path(&self, model: &TemperModel, mut at: usize) -> Vec<String> {
        let mut moves = Vec::new();
        while let Some((from, t)) = self.parent[at] {
            moves.push(model.transitions[t as usize].name.clone());
            at = from as usize;
        }
        moves.reverse();
        moves
    }
}

/// Places as a set of bits, one word per 64 places.
#[derive(Clone, Copy)]
struct Bits {
    words: usize,
}

impl Bits {
    fn row<'a>(&self, all: &'a [u64], i: usize) -> &'a [u64] {
        &all[i * self.words..(i + 1) * self.words]
    }

    fn names(&self, model: &TemperModel, set: &[u64]) -> Vec<String> {
        (0..model.states.len())
            .filter(|&p| set[p / 64] & (1 << (p % 64)) != 0)
            .map(|p| model.states[p].clone())
            .collect()
    }
}

/// Every record the walk in `graph` found that loses places, one per place
/// and set of places lost, shortest paths first. `found` maps each state to
/// its number in `graph`.
pub(crate) fn lost_places(
    model: &TemperModel,
    graph: &Graph,
    found: &HashMap<TemperModelState, u32>,
) -> Vec<LostPlace> {
    let n = graph.len();
    if n == 0 || graph.offsets.len() != n + 1 {
        return Vec::new();
    }
    let bits = Bits {
        words: model.states.len().div_ceil(64).max(1),
    };
    let (component, count) = components(graph);

    // What each component can reach, and whether a step past a bound lies
    // ahead of it. A component is numbered after every one it steps to.
    let mut first = vec![0usize; count + 1];
    for &c in &component {
        first[c as usize + 1] += 1;
    }
    for c in 0..count {
        first[c + 1] += first[c];
    }
    let mut members = vec![0usize; n];
    let mut next = first.clone();
    for (at, &c) in component.iter().enumerate() {
        members[next[c as usize]] = at;
        next[c as usize] += 1;
    }
    let mut reach = vec![0u64; count * bits.words];
    let mut past_bound = vec![false; count];
    for c in 0..count {
        let mut row = vec![0u64; bits.words];
        let mut cut = false;
        for &at in &members[first[c]..first[c + 1]] {
            let place = graph.place[at];
            if place != NO_PLACE {
                row[place as usize / 64] |= 1 << (place % 64);
            }
            cut |= graph.cut[at];
            for &to in graph.steps(at) {
                let d = component[to as usize] as usize;
                if d != c {
                    for (w, word) in row.iter_mut().enumerate() {
                        *word |= bits.row(&reach, d)[w];
                    }
                    cut |= past_bound[d];
                }
            }
        }
        reach[c * bits.words..(c + 1) * bits.words].copy_from_slice(&row);
        past_bound[c] = cut;
    }
    let reach_of = |at: usize| bits.row(&reach, component[at] as usize);

    // What some record in each place can reach.
    let mut anyone = vec![0u64; model.states.len() * bits.words];
    for at in 0..n {
        let place = graph.place[at];
        if place != NO_PLACE {
            let start = place as usize * bits.words;
            for (w, word) in reach_of(at).iter().enumerate() {
                anyone[start + w] |= word;
            }
        }
    }

    // Each state, its whole future walked, that cannot reach what another
    // in its place can.
    let mut lost: BTreeMap<usize, Vec<u64>> = BTreeMap::new();
    for at in 0..n {
        let place = graph.place[at];
        if place == NO_PLACE {
            continue;
        }
        let row: Vec<u64> = bits
            .row(&anyone, place as usize)
            .iter()
            .zip(reach_of(at))
            .map(|(all, mine)| all & !mine)
            .collect();
        if row.iter().any(|w| *w != 0) {
            lost.insert(at, row);
        }
    }

    // A loss is made by a move from a state that could still reach some of
    // it, or is there from the start.
    let mut made_here: BTreeSet<usize> = lost
        .keys()
        .copied()
        .filter(|&at| graph.parent[at].is_none())
        .collect();
    for from in 0..n {
        for &to in graph.steps(from) {
            let to = to as usize;
            if let Some(row) = lost.get(&to)
                && reach_of(from).iter().zip(row).any(|(r, l)| r & l != 0)
            {
                made_here.insert(to);
            }
        }
    }

    // One finding per place and set of places lost, with its shortest
    // record and the other record in its place that reaches the most of them.
    let mut groups: BTreeMap<(u32, Vec<u64>), usize> = BTreeMap::new();
    for &at in &made_here {
        let key = (graph.place[at], lost[&at].clone());
        let best = groups.entry(key).or_insert(at);
        if (graph.depth[at], at) < (graph.depth[*best], *best) {
            *best = at;
        }
    }
    let mut findings: Vec<(usize, usize, Vec<u64>)> = groups
        .into_iter()
        .map(|((place, row), at)| {
            let shared = |other: usize| -> u32 {
                reach_of(other)
                    .iter()
                    .zip(&row)
                    .map(|(r, l)| (r & l).count_ones())
                    .sum()
            };
            let other = (0..n)
                .filter(|&o| graph.place[o] == place && shared(o) > 0)
                .min_by_key(|&o| (std::cmp::Reverse(shared(o)), graph.depth[o], o))
                .expect("some record in the place reaches what this one lost");
            (at, other, row)
        })
        .collect();
    findings.sort_by_key(|(at, _, _)| (graph.depth[*at], *at));

    let wanted: BTreeSet<u32> = findings
        .iter()
        .flat_map(|(at, other, _)| [*at as u32, *other as u32])
        .collect();
    let states: BTreeMap<u32, &TemperModelState> = found
        .iter()
        .filter(|(_, at)| wanted.contains(at))
        .map(|(state, at)| (*at, state))
        .collect();
    findings
        .into_iter()
        .map(|(at, other, row)| {
            let other_reaches: Vec<u64> = reach_of(other)
                .iter()
                .zip(&row)
                .map(|(r, l)| r & l)
                .collect();
            LostPlace {
                place: model.states[graph.place[at] as usize].clone(),
                lost: bits.names(model, &row),
                record: states[&(at as u32)].clone(),
                path: graph.path(model, at),
                other: states[&(other as u32)].clone(),
                other_path: graph.path(model, other),
                other_reaches: bits.names(model, &other_reaches),
            }
        })
        .collect()
}

/// The strongly connected components of `graph` (Tarjan's, without
/// recursion): each state's component, numbered after every component it
/// steps to, and how many there are.
fn components(graph: &Graph) -> (Vec<u32>, usize) {
    let n = graph.len();
    let mut t = Tarjan {
        order: vec![UNSEEN; n],
        low: vec![0; n],
        open: Vec::new(),
        calls: Vec::new(),
        seen: 0,
    };
    let mut component = vec![UNSEEN; n];
    let mut count = 0u32;
    for root in 0..n {
        if t.order[root] != UNSEEN {
            continue;
        }
        t.enter(graph, root as u32);
        while let Some(&(v, next)) = t.calls.last() {
            let vi = v as usize;
            if next < graph.offsets[vi + 1] {
                t.calls.last_mut().expect("a call is open").1 += 1;
                let w = graph.steps[next as usize] as usize;
                if t.order[w] == UNSEEN {
                    t.enter(graph, w as u32);
                } else if component[w] == UNSEEN {
                    // Still open: on the path or in a component not yet closed.
                    t.low[vi] = t.low[vi].min(t.order[w]);
                }
                continue;
            }
            t.calls.pop();
            if let Some(&(u, _)) = t.calls.last() {
                t.low[u as usize] = t.low[u as usize].min(t.low[vi]);
            }
            if t.low[vi] == t.order[vi] {
                loop {
                    let w = t.open.pop().expect("v is still open");
                    component[w as usize] = count;
                    if w == v {
                        break;
                    }
                }
                count += 1;
            }
        }
    }
    (component, count as usize)
}

const UNSEEN: u32 = u32::MAX;

/// The walk state of [`components`].
struct Tarjan {
    /// The order each state was entered in.
    order: Vec<u32>,
    /// The earliest entered open state each state's walk reaches.
    low: Vec<u32>,
    /// States entered whose component is not closed yet.
    open: Vec<u32>,
    /// The states being walked, each with the next of its steps to take.
    calls: Vec<(u32, u32)>,
    seen: u32,
}

impl Tarjan {
    fn enter(&mut self, graph: &Graph, v: u32) {
        self.order[v as usize] = self.seen;
        self.low[v as usize] = self.seen;
        self.seen += 1;
        self.open.push(v);
        self.calls.push((v, graph.offsets[v as usize]));
    }
}

/// The fields in which `a` and `b` differ, as "makers = 1" for each.
fn differences(a: &TemperModelState, b: &TemperModelState) -> (String, String) {
    let mut mine = Vec::new();
    let mut theirs = Vec::new();
    let mut note = |name: &str, x: String, y: String| {
        if x != y {
            mine.push(format!("{name} = {x}"));
            theirs.push(format!("{name} = {y}"));
        }
    };
    for (name, x) in &a.counters {
        note(name, x.to_string(), field(b.counters.get(name)));
    }
    for (name, x) in &a.booleans {
        note(name, x.to_string(), field(b.booleans.get(name)));
    }
    for (name, x) in &a.lists {
        let y = b.lists.get(name).map(|y| format!("{y:?}"));
        note(name, format!("{x:?}"), y.unwrap_or_default());
    }
    for (name, x) in &a.strings {
        note(name, x.to_string(), field(b.strings.get(name)));
    }
    if mine.is_empty() {
        return ("the same fields".into(), "the same fields".into());
    }
    (mine.join(" and "), theirs.join(" and "))
}

fn field(value: Option<&impl ToString>) -> String {
    value.map(ToString::to_string).unwrap_or_default()
}

/// "A", "A or B", "A, B or C".
fn either(places: &[String]) -> String {
    match places {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// "after Add > Claim", with a run of the same moves written once with its
/// count: "after Start > (Call > Answer) x 100".
fn after(path: &[String]) -> String {
    if path.is_empty() {
        return "at the start".into();
    }
    let mut parts = Vec::new();
    let mut i = 0;
    while i < path.len() {
        // The repeated run that covers the most moves from here.
        let mut best = (1, 1);
        for period in 1..=4.min(path.len() - i) {
            let unit = &path[i..i + period];
            let mut times = 1;
            while path[i + times * period..].starts_with(unit) {
                times += 1;
            }
            if times > 1 && period * times > best.0 * best.1 {
                best = (period, times);
            }
        }
        let (period, times) = best;
        let unit = path[i..i + period].join(" > ");
        parts.push(match (period, times) {
            (_, 1) => unit,
            (1, _) => format!("{unit} x {times}"),
            _ => format!("({unit}) x {times}"),
        });
        i += period * times;
    }
    format!("after {}", parts.join(" > "))
}
