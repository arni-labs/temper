//! `reaches` rules: every record in one of a rule's `from` places (a new
//! record when it names none) gets to one of the places the rule names,
//! whatever moves it makes.
//!
//! Checked on the model check's whole walk, one rule at a time, as a
//! backward fixpoint: a state gets there when it is in a named place, or
//! when it has a step and every step leads to a state that gets there. A
//! state left over can stop, or go round a loop, without ever getting there.
//! The spec language has no fairness, so such a loop breaks the rule as
//! surely as a state with no way out.
//!
//! A state with a step past a counter's bound is not taken for a stop: what
//! lies past the bound was not walked, and the walk's result says it is
//! bounded. A walk cut short by its time or state limit checks nothing here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::can_reach::trace;
use crate::checker::Counterexample;
use crate::model::semantics::successors;
use crate::model::{LivenessKind, TemperModel, TemperModelState};
use crate::reach::{Graph, NO_PLACE};

/// The property a broken `reaches` rule is reported under; the rule's name
/// is what is false.
pub const REACHES: &str = "Reaches";

/// One counterexample for each `reaches` rule some path in `graph` breaks:
/// the path to the nearest state in the rule's `from` places that need not
/// get there, then on through states that need not either, until it stops
/// or comes back to a state already on it. `found` maps each state to its
/// number in `graph`.
pub(crate) fn broken_rules(
    model: &TemperModel,
    graph: &Graph,
    found: &HashMap<TemperModelState, u32>,
) -> Vec<Counterexample> {
    let rules: Vec<(&str, &[String], &[String])> = model
        .liveness
        .iter()
        .filter_map(|live| match &live.kind {
            LivenessKind::ReachesState { from, targets } if !targets.is_empty() => {
                Some((live.name.as_str(), from.as_slice(), targets.as_slice()))
            }
            _ => None,
        })
        .collect();
    if rules.is_empty() {
        return Vec::new();
    }
    // The whole walk is complete. Replaying its evidence must neither spend
    // its remaining search budget nor stop when its deadline passes.
    let replay = TemperModel {
        limit: None,
        ..model.clone()
    };

    let n = graph.len();
    let mut into: Vec<Vec<u32>> = vec![Vec::new(); n];
    for at in 0..n {
        for &to in graph.steps(at) {
            into[to as usize].push(at as u32);
        }
    }
    let in_places = |names: &[String]| -> Vec<bool> {
        let places: Vec<bool> = model.states.iter().map(|s| names.contains(s)).collect();
        (0..n)
            .map(|at| graph.place[at] != NO_PLACE && places[graph.place[at] as usize])
            .collect()
    };

    let broken: Vec<Broken> = rules
        .into_iter()
        .filter_map(|(name, from, targets)| {
            let arrives = gets_there(graph, &into, in_places(targets));
            let starts = if from.is_empty() {
                (0..n).map(|at| graph.parent[at].is_none()).collect()
            } else {
                in_places(from)
            };
            // The walk numbers states in the order it finds them, so the
            // first that breaks the rule is one of the nearest.
            let at = (0..n).find(|&at| starts[at] && !arrives[at])?;
            Some(Broken {
                name: name.to_string(),
                targets: targets.to_vec(),
                path: path_that_never_arrives(&replay, graph, found, &arrives, at),
            })
        })
        .collect();
    if broken.is_empty() {
        return Vec::new();
    }

    let wanted: BTreeSet<u32> = broken
        .iter()
        .flat_map(|broken| broken.path.iter().map(|(at, _)| *at as u32))
        .collect();
    let states: BTreeMap<usize, &TemperModelState> = found
        .iter()
        .filter(|(_, at)| wanted.contains(at))
        .map(|(state, at)| (*at as usize, state))
        .collect();
    broken
        .into_iter()
        .map(|broken| Counterexample {
            property: REACHES.to_string(),
            violated: vec![broken.name],
            trace: trace(&replay, &states, &broken.path),
            never_reaches: broken.targets,
        })
        .collect()
}

/// A broken rule: its name, its places, and the path that never gets
/// there, each state's number with the transition taken from it.
struct Broken {
    name: String,
    targets: Vec<String>,
    path: Vec<(usize, Option<usize>)>,
}

/// Which states every path from gets to a state in `arrived`: the least
/// fixpoint of "in `arrived`, or has a step and every step leads to one
/// that gets there", where a state with a step past a bound and no other
/// counts as getting there (what lies past the bound was not walked).
fn gets_there(graph: &Graph, into: &[Vec<u32>], mut arrived: Vec<bool>) -> Vec<bool> {
    let n = graph.len();
    let mut left: Vec<usize> = (0..n).map(|at| graph.steps(at).len()).collect();
    let mut queue: Vec<usize> = Vec::new();
    for at in 0..n {
        if !arrived[at] && left[at] == 0 && graph.cut[at] {
            arrived[at] = true;
        }
        if arrived[at] {
            queue.push(at);
        }
    }
    while let Some(at) = queue.pop() {
        for &from in &into[at] {
            let from = from as usize;
            if arrived[from] {
                continue;
            }
            left[from] -= 1;
            if left[from] == 0 {
                arrived[from] = true;
                queue.push(from);
            }
        }
    }
    arrived
}

/// The walk's path to `at`, then on through states that need not get
/// there (each has such a step unless it has none at all) until one has no
/// step or the next is already on the path. Each state's number with the
/// transition taken from it, the last with none.
fn path_that_never_arrives(
    model: &TemperModel,
    graph: &Graph,
    found: &HashMap<TemperModelState, u32>,
    arrives: &[bool],
    at: usize,
) -> Vec<(usize, Option<usize>)> {
    let mut path = graph.path_to(at);
    let mut on_path: BTreeSet<usize> = path.iter().map(|(at, _)| *at).collect();
    let number: HashMap<u32, &TemperModelState> = found.iter().map(|(s, at)| (*at, s)).collect();
    loop {
        let here = path.last().expect("a path has a state").0;
        let onward: Vec<usize> = graph
            .steps(here)
            .iter()
            .map(|&to| to as usize)
            .filter(|&to| !arrives[to])
            .collect();
        // A step back onto the path closes the loop soonest.
        let Some(next) = onward
            .iter()
            .copied()
            .find(|to| on_path.contains(to))
            .or_else(|| onward.first().copied())
        else {
            return path;
        };
        let transition = transition_between(model, number[&(here as u32)], number[&(next as u32)]);
        path.last_mut().expect("a path has a state").1 = Some(transition);
        path.push((next, None));
        if !on_path.insert(next) {
            return path;
        }
    }
}

/// The index of a transition that steps `from` to `to`.
fn transition_between(
    model: &TemperModel,
    from: &TemperModelState,
    to: &TemperModelState,
) -> usize {
    model
        .transitions
        .iter()
        .position(|transition| {
            model.transition_enabled(transition, from)
                && successors(model, transition, from)
                    .iter()
                    .any(|(_, next)| next == to)
        })
        .expect("the walk took this step")
}
