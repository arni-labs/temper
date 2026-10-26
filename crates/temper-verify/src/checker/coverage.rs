//! The search for transitions no reachable state enables.
//!
//! It ends once every transition has fired, so it takes next the state
//! nearest, in the status graph, to a status where a transition still
//! unfired may start, the latest found first among the nearest. A
//! transition is named dead only after every reachable state is searched,
//! so the order finds the live ones sooner and changes no verdict.

use std::collections::{BTreeMap, HashMap, HashSet};

use stateright::Model;

use crate::model::ResolvedTransition;
use crate::model::semantics::{guard_may_hold, successors};
use crate::model::{TemperModel, TemperModelState};

/// What the search for dead transitions found.
pub(super) struct Coverage {
    /// The transitions no reachable state enables.
    pub(super) dead: Vec<String>,
    /// The states it expanded.
    pub(super) states: usize,
}

/// The transitions no reachable state enables, or `None` when the model's
/// limit ran out first.
pub(super) fn find_dead_transitions(model: &TemperModel) -> Option<Coverage> {
    let mut covered = vec![false; model.transitions.len()];
    let mut unfired = covered.len();
    let mut near = nearness(model, &covered);
    let mut visited = HashSet::new();
    let mut frontier = Frontier::default();
    for init in model.init_states() {
        if visited.insert(init.clone()) {
            frontier.push(&near, init);
        }
    }

    let (mut expanded, mut generated) = (0, 0);
    while unfired > 0
        && let Some(state) = frontier.pop(&near)
    {
        // The state limit bounds this search's memory as it does the walk's.
        let full = model
            .limit
            .as_ref()
            .is_some_and(|limit| generated >= limit.states);
        if model.out_of_time() || full {
            return None;
        }
        expanded += 1;
        let mut fired = false;
        for (index, transition) in model.transitions.iter().enumerate() {
            let next_states = step(model, transition, &state);
            if next_states.is_empty() {
                continue;
            }
            if !covered[index] {
                covered[index] = true;
                unfired -= 1;
                fired = true;
            }
            generated += next_states.len();
            for next in next_states {
                if visited.insert(next.clone()) {
                    frontier.push(&near, next);
                }
            }
        }
        if fired {
            near = nearness(model, &covered);
        }
    }

    let dead = model
        .transitions
        .iter()
        .zip(&covered)
        .filter(|(_, covered)| !**covered)
        .map(|(transition, _)| render_transition_label(transition))
        .collect();
    Some(Coverage {
        dead,
        states: expanded,
    })
}

/// How many transitions each status is from one where a transition not yet
/// `covered` may start. A status it leaves out leads to none.
fn nearness<'m>(model: &'m TemperModel, covered: &[bool]) -> HashMap<&'m str, usize> {
    let unfired: Vec<&ResolvedTransition> = model
        .transitions
        .iter()
        .zip(covered)
        .filter(|(_, covered)| !**covered)
        .map(|(transition, _)| transition)
        .collect();
    // A transition that names no status starts anywhere.
    model.distances(|status| {
        unfired
            .iter()
            .any(|t| t.from_states.is_empty() || t.from_states.iter().any(|f| f == status))
    })
}

/// The states found and not yet expanded, filed by how near they were.
#[derive(Default)]
struct Frontier(BTreeMap<usize, Vec<TemperModelState>>);

impl Frontier {
    fn distance(near: &HashMap<&str, usize>, state: &TemperModelState) -> usize {
        near.get(state.status.as_str())
            .copied()
            .unwrap_or(usize::MAX)
    }

    fn push(&mut self, near: &HashMap<&str, usize>, state: TemperModelState) {
        let distance = Self::distance(near, &state);
        self.0.entry(distance).or_default().push(state);
    }

    /// The nearest state as `near` reads it now: one filed before a
    /// transition fired may have moved further away since.
    fn pop(&mut self, near: &HashMap<&str, usize>) -> Option<TemperModelState> {
        loop {
            let mut nearest = self.0.first_entry()?;
            let filed = *nearest.key();
            let Some(state) = nearest.get_mut().pop() else {
                nearest.remove();
                continue;
            };
            if Self::distance(near, &state) > filed {
                self.push(near, state);
                continue;
            }
            return Some(state);
        }
    }
}

/// The states `transition` reaches from `state`: none when it is not
/// enabled, one per explored parameter assignment otherwise.
pub(super) fn step(
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
