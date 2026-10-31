//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

use std::collections::{BTreeMap, HashMap, VecDeque};

use stateright::{Checker, Expectation, Model};
use temper_spec::predicate::Truth;

use crate::can_reach::broken_rules;
use crate::lost_places::{LostPlace, lost_places};
use crate::model::ResolvedTransition;
use crate::model::semantics::{successors_noting_cuts, truth};
use crate::model::{TemperModel, TemperModelAction, TemperModelState};
use crate::reach::{Graph, Reach};

/// A counterexample discovered during model checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Counterexample {
    /// The property name that was violated.
    pub property: String,
    /// What is false in the trace's last state: for `Invariants`, the names
    /// of the invariants false there; for any other property, its name.
    #[serde(default)]
    pub violated: Vec<String>,
    /// The sequence of (state, action) pairs leading to the violation: each
    /// state and the action taken from it, the last state with none. For a
    /// safety property it ends at the first state on the path that breaks it.
    pub trace: Vec<(TemperModelState, Option<TemperModelAction>)>,
    /// For a broken `can_reach` rule, the places the trace's last state can
    /// never reach; empty for any other property.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub never_reaches: Vec<String>,
}

/// The result of running exhaustive model checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerificationResult {
    /// Total number of unique states explored.
    pub states_explored: usize,
    /// Whether all declared properties hold across all reachable states.
    pub all_properties_hold: bool,
    /// Counterexamples found (one per violated property).
    pub counterexamples: Vec<Counterexample>,
    /// Transitions declared in the model that were never enabled on any reachable state.
    pub dead_transitions: Vec<String>,
    /// Whether the checker completed its exploration (vs. hitting a limit).
    pub is_complete: bool,
    /// The counters and lists a reachable move takes past the bound they
    /// are explored to, each with that bound. Those moves are not explored,
    /// so when this is not empty every property holds only up to these
    /// bounds: the result is bounded, not proved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bounded_by: Vec<(String, usize)>,
    /// Records that can never reach a place another record in the same
    /// place still can, found only by a whole walk. Worth a look; they
    /// don't fail the check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lost_places: Vec<LostPlace>,
}

/// Run exhaustive BFS model checking on the given `TemperModel`.
///
/// This spawns Stateright's BFS checker, joins it, and then inspects the
/// discoveries to build a `VerificationResult`. A model with a budget's
/// limit stops at its time or state limit and reports itself incomplete.
pub fn check_model(model: &TemperModel) -> VerificationResult {
    // The checker's own copy of the model, whose limit is lifted once the
    // search is over: Stateright rebuilds each found path by stepping it.
    let walked = TemperModel {
        limit: model.limit.as_ref().map(|limit| limit.for_one_walk()),
        ..model.clone()
    };
    let mut checker = walked.clone().checker();
    if let Some(limit) = &model.limit {
        checker = checker.target_state_count(limit.states);
    }
    let checker_result = checker.spawn_bfs().join();

    let states_explored = checker_result.unique_state_count();
    // Neither limit shows in `is_done`: a checker thread that stops at its
    // target state count closes the job market as it exits, and a spent time
    // limit ends the BFS normally because the model stopped offering actions.
    let cut_by_time = walked.out_of_time();
    let explored_all = checker_result.is_done()
        && !cut_by_time
        && model
            .limit
            .as_ref()
            .is_none_or(|limit| checker_result.state_count() < limit.states);
    if let Some(limit) = &walked.limit {
        limit.lift();
    }

    let discoveries = checker_result.discoveries();
    let mut counterexamples = Vec::new();

    for (property_name, path) in discoveries {
        // After the time cut every state still queued had no actions, so it
        // looks like the end of a walk: an `eventually` property found false
        // there proves nothing. (A state-limit stop leaves the queue as it was.)
        if cut_by_time && property_name == crate::model::REACHES_TERMINAL {
            continue;
        }
        let trace = first_break(&walked, property_name, path.into_vec());
        let violated = match trace.last() {
            Some((state, _)) if property_name == "Invariants" => walked
                .invariants
                .iter()
                .filter(|inv| truth(&inv.assert, &walked.var_kinds, state) == Truth::False)
                .map(|inv| inv.name.clone())
                .collect(),
            _ => vec![property_name.to_string()],
        };
        counterexamples.push(Counterexample {
            property: property_name.to_string(),
            violated,
            trace,
            never_reaches: Vec::new(),
        });
    }

    // An edge never seen in part of the state space may still be enabled in
    // the rest: dead transitions are named only after a whole exploration.
    let walked = if explored_all { walk(model) } else { None };
    let is_complete = walked.is_some();
    let (dead_transitions, bounded_by, lost_places) = match walked {
        Some(w) => {
            counterexamples.extend(w.broken);
            (w.dead, w.cut_short.into_iter().collect(), w.lost_places)
        }
        None => Default::default(),
    };
    let all_properties_hold =
        is_complete && counterexamples.is_empty() && dead_transitions.is_empty();

    VerificationResult {
        states_explored,
        all_properties_hold,
        counterexamples,
        dead_transitions,
        is_complete,
        bounded_by,
        lost_places,
    }
}

/// `steps` cut at the first state that breaks the safety property `name`.
///
/// Stateright keeps exploring after a safety property first fails (other
/// properties still await theirs) and records each later state that fails
/// it over the earlier one, so the path it returns ends at the last such
/// state it visited, not the first: on a 23-move break it returned 38 moves.
/// Cutting at the first failing state gives a path that ends where the
/// property broke. A liveness (`eventually`) path is returned whole.
fn first_break(
    model: &TemperModel,
    name: &str,
    mut steps: Vec<(TemperModelState, Option<TemperModelAction>)>,
) -> Vec<(TemperModelState, Option<TemperModelAction>)> {
    let Some(property) = model
        .properties()
        .into_iter()
        .find(|p| p.name == name && p.expectation == Expectation::Always)
    else {
        return steps;
    };
    if let Some(first) = steps
        .iter()
        .position(|(state, _)| !(property.condition)(model, state))
    {
        steps.truncate(first + 1);
        steps[first].1 = None;
    }
    steps
}

/// What a walk of every reachable state found.
struct Walked {
    /// The transitions no reachable state enables.
    dead: Vec<String>,
    /// The counters and lists a reachable move takes past their bound, with it.
    cut_short: BTreeMap<String, usize>,
    /// The records that can never reach a place another in theirs can.
    lost_places: Vec<LostPlace>,
    /// A path for each `can_reach` rule a record breaks.
    broken: Vec<Counterexample>,
}

/// Walk every reachable state, or return `None` when the model's time limit
/// ran out before every reachable state was walked.
fn walk(model: &TemperModel) -> Option<Walked> {
    let mut found: HashMap<TemperModelState, u32> = HashMap::new();
    let mut graph = Graph::default();
    let mut queue = VecDeque::new();
    for init in model.init_states() {
        if !found.contains_key(&init) {
            let at = graph.add(model, &init, None);
            found.insert(init.clone(), at);
            queue.push_back((init, at));
        }
    }

    let mut covered = vec![false; model.transitions.len()];
    let mut cut = BTreeMap::new();

    while let Some((state, at)) = queue.pop_front() {
        if model.out_of_time() {
            return None;
        }
        let mut cut_here = BTreeMap::new();
        let mut to = Vec::new();
        for (index, transition) in model.transitions.iter().enumerate() {
            let next_states = step(model, transition, &state, &mut cut_here);
            if model.out_of_time() {
                return None;
            }
            if next_states.is_empty() {
                continue;
            }
            covered[index] = true;
            for next in next_states {
                let next_at = match found.get(&next) {
                    Some(&next_at) => next_at,
                    None => {
                        let next_at = graph.add(model, &next, Some((at, index as u32)));
                        found.insert(next.clone(), next_at);
                        queue.push_back((next, next_at));
                        next_at
                    }
                };
                to.push(next_at);
            }
        }
        graph.steps_from(at, !cut_here.is_empty(), to);
        cut.extend(cut_here);
    }

    let dead = model
        .transitions
        .iter()
        .enumerate()
        .filter_map(|(index, transition)| {
            if covered[index] {
                None
            } else {
                Some(render_transition_label(transition))
            }
        })
        .collect();
    let (lost_places, broken) = match Reach::of(model, &graph) {
        Some(reach) => (
            lost_places(model, &graph, &reach, &found),
            broken_rules(model, &graph, &reach, &found),
        ),
        None => Default::default(),
    };
    Some(Walked {
        dead,
        cut_short: cut,
        lost_places,
        broken,
    })
}

/// The states `transition` reaches from `state`: none when it is not
/// enabled, one per explored parameter assignment otherwise. Adds to `cut`
/// what a step it leaves out would take past its bound.
fn step(
    model: &TemperModel,
    transition: &ResolvedTransition,
    state: &TemperModelState,
    cut: &mut BTreeMap<String, usize>,
) -> Vec<TemperModelState> {
    // `transition_enabled` admits a cross-entity gated edge (its guard is a
    // free boolean), so the gated target state is walked, as it is reachable
    // in the model; a transition another with its name shadows is not.
    if !model.transition_enabled(transition, state) {
        return Vec::new();
    }
    successors_noting_cuts(model, transition, state, cut)
        .into_iter()
        .map(|(_, next)| next)
        .collect()
}

fn render_transition_label(transition: &ResolvedTransition) -> String {
    let from = if transition.from_states.is_empty() {
        "*".to_string()
    } else {
        transition.from_states.join("|")
    };
    let to = transition
        .to_state
        .clone()
        .unwrap_or_else(|| "<same>".to_string());
    format!("{} [{} -> {}]", transition.name, from, to)
}

#[cfg(test)]
#[path = "checker_test.rs"]
mod tests;
