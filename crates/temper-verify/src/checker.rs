//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

use std::collections::{HashSet, VecDeque};

use stateright::{Checker, Expectation, Model};
use temper_spec::predicate::Truth;

use crate::model::ResolvedTransition;
use crate::model::semantics::{guard_may_hold, successors, truth};
use crate::model::{TemperModel, TemperModelAction, TemperModelState};

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
        });
    }

    // An edge never seen in part of the state space may still be enabled in
    // the rest: dead transitions are named only after a whole exploration.
    let dead_transitions = if explored_all {
        find_dead_transitions(model)
    } else {
        None
    };
    let is_complete = dead_transitions.is_some();
    let dead_transitions = dead_transitions.unwrap_or_default();
    let all_properties_hold =
        is_complete && counterexamples.is_empty() && dead_transitions.is_empty();

    VerificationResult {
        states_explored,
        all_properties_hold,
        counterexamples,
        dead_transitions,
        is_complete,
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

/// The transitions no reachable state enables, or `None` when the model's
/// time limit ran out before every reachable state was walked.
fn find_dead_transitions(model: &TemperModel) -> Option<Vec<String>> {
    let mut visited_states = HashSet::new();
    let mut queue = VecDeque::new();
    for init in model.init_states() {
        if visited_states.insert(init.clone()) {
            queue.push_back(init);
        }
    }

    let mut covered = vec![false; model.transitions.len()];

    while let Some(state) = queue.pop_front() {
        if model.out_of_time() {
            return None;
        }
        for (index, transition) in model.transitions.iter().enumerate() {
            let next_states = step(model, transition, &state);
            if model.out_of_time() {
                return None;
            }
            if next_states.is_empty() {
                continue;
            }
            covered[index] = true;
            for next in next_states {
                if visited_states.insert(next.clone()) {
                    queue.push_back(next);
                }
            }
        }
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
    Some(dead)
}

/// The states `transition` reaches from `state`: none when it is not
/// enabled, one per explored parameter assignment otherwise.
fn step(
    model: &TemperModel,
    transition: &ResolvedTransition,
    state: &TemperModelState,
) -> Vec<TemperModelState> {
    let status_ok = transition.from_states.is_empty()
        || transition
            .from_states
            .iter()
            .any(|from| from == &state.status);
    // Use `guard_may_hold` (not `evaluate_guard`) so cross-entity gated edges
    // are walked during reachability BFS: a cross-entity guard is a free
    // boolean, so the gated target state is genuinely reachable in the model.
    if !status_ok || !guard_may_hold(&transition.guard, &model.var_kinds, state) {
        return Vec::new();
    }
    successors(model, transition, state)
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
