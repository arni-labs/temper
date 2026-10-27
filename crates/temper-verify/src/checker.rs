//! Run exhaustive model checking on a `TemperModel`.
//!
//! This module wraps Stateright's BFS model checker and produces a
//! `VerificationResult` summarizing the outcome.

mod coverage;

use stateright::{Checker, Model};

use crate::model::{TemperModel, TemperModelAction, TemperModelState};
use coverage::find_dead_transitions;

/// A counterexample discovered during model checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Counterexample {
    /// The property name that was violated.
    pub property: String,
    /// The sequence of (state, action) pairs leading to the violation.
    pub trace: Vec<(TemperModelState, Option<TemperModelAction>)>,
}

/// The result of running exhaustive model checking.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct VerificationResult {
    /// Unique states the walk explored, or, when no property needed a walk,
    /// the states the search for dead transitions expanded.
    pub states_explored: usize,
    /// Whether all declared properties hold across all reachable states.
    pub all_properties_hold: bool,
    /// Counterexamples found (one per violated property).
    pub counterexamples: Vec<Counterexample>,
    /// Transitions declared in the model that were never enabled on any reachable state.
    pub dead_transitions: Vec<String>,
    /// Whether the checker decided every property before hitting a limit:
    /// it walked every reachable state, or no property needed a walk and
    /// every transition fired (or every state was searched for them).
    pub is_complete: bool,
}

/// Run exhaustive BFS model checking on the given `TemperModel`.
///
/// Stateright's BFS walks the reachable states when a property needs it,
/// and a second search names the transitions no reachable state enables. A
/// model with a budget's limit stops at its time or state limit and reports
/// itself incomplete.
pub fn check_model(model: &TemperModel) -> VerificationResult {
    let walk = needs_walk(model).then(|| walk(model));
    // An edge never seen in part of the state space may still be enabled in
    // the rest: dead transitions are named only after a whole exploration.
    let coverage = if walk.as_ref().is_none_or(|walk| walk.explored_all) {
        find_dead_transitions(model)
    } else {
        None
    };
    let is_complete = coverage.is_some();
    let (states_explored, counterexamples) = match walk {
        Some(walk) => (walk.states, walk.counterexamples),
        None => (coverage.as_ref().map_or(0, |c| c.states), Vec::new()),
    };
    let dead_transitions = coverage.map(|c| c.dead).unwrap_or_default();
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

/// Whether a property needs the reachable states walked. Invariants and
/// liveness rules do; the status and terminal checks do only when the
/// spec's shape leaves them open.
fn needs_walk(model: &TemperModel) -> bool {
    model
        .properties()
        .iter()
        .any(|property| match property.name {
            "TypeInvariant" => !statuses_declared(model),
            "Terminal" => may_leave_terminal(model),
            _ => true,
        })
}

/// Whether every status a walk can reach is declared: the initial one and
/// each one a transition leads to.
fn statuses_declared(model: &TemperModel) -> bool {
    std::iter::once(&model.initial_status)
        .chain(model.transitions.iter().filter_map(|t| t.to_state.as_ref()))
        .all(|status| model.states.contains(status))
}

/// Whether some transition may start in a terminal status: one that names
/// a terminal status, or names none and so starts anywhere.
fn may_leave_terminal(model: &TemperModel) -> bool {
    !model.terminal.is_empty()
        && model.transitions.iter().any(|t| {
            t.from_states.is_empty() || t.from_states.iter().any(|s| model.terminal.contains(s))
        })
}

/// What Stateright's walk found.
struct Walk {
    states: usize,
    counterexamples: Vec<Counterexample>,
    /// Whether it walked every reachable state.
    explored_all: bool,
}

fn walk(model: &TemperModel) -> Walk {
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

    let states = checker_result.unique_state_count();
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

    let mut counterexamples = Vec::new();
    for (property_name, path) in checker_result.discoveries() {
        // After the time cut every state still queued had no actions, so it
        // looks like the end of a walk: an `eventually` property found false
        // there proves nothing. (A state-limit stop leaves the queue as it was.)
        if cut_by_time && property_name == crate::model::REACHES_TERMINAL {
            continue;
        }
        counterexamples.push(Counterexample {
            property: property_name.to_string(),
            trace: path.into_vec(),
        });
    }
    Walk {
        states,
        counterexamples,
        explored_all,
    }
}

#[cfg(test)]
#[path = "checker_test.rs"]
mod tests;
