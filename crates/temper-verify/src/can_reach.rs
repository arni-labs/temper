//! `can_reach` rules: a record in one of a rule's `from` places can always
//! still reach one of the places it names.
//!
//! Checked on the model check's whole walk, from what each state can reach.
//! A state with a step past a counter's bound ahead of it never breaks a
//! rule: what lies past the bound was not walked, so the pass is bounded,
//! not a refusal.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use temper_spec::predicate::{Name, VarKind};

use crate::checker::Counterexample;
use crate::model::semantics::successors;
use crate::model::{LivenessKind, TemperModel, TemperModelAction, TemperModelState};
use crate::reach::{Graph, NO_PLACE, Reach};

/// The property a broken `can_reach` rule is reported under; the rule's
/// name is what is false.
pub const CAN_REACH: &str = "CanReach";

/// A possible edge through an unmodeled local numeric value cannot prove
/// that a route exists. Until those values are modeled, conservatively leave
/// the rules unproved when any guard reads one. This does not suppress a
/// concrete refusal found in the overapproximated graph, nor change how
/// cross-entity guards are explored.
pub(crate) fn unproved_rules(model: &TemperModel) -> Vec<String> {
    let mut numeric = BTreeSet::new();
    for transition in &model.transitions {
        transition.guard.for_each_name(&mut |name| {
            if let Name::Var(var) = name
                && model.var_kinds.get(var) == Some(&VarKind::Num)
            {
                numeric.insert(var.to_string());
            }
        });
    }
    if numeric.is_empty() {
        return Vec::new();
    }
    let names = numeric.into_iter().collect::<Vec<_>>().join(", ");
    model
        .liveness
        .iter()
        .filter(|live| matches!(live.kind, LivenessKind::CanReach { .. }))
        .map(|live| {
            format!(
                "rule '{}' not proved: guards read unmodeled local numeric variables: {names}",
                live.name
            )
        })
        .collect()
}

/// One counterexample for each `can_reach` rule a state in `graph` breaks,
/// whose path leads to the nearest such state. `found` maps each state to
/// its number in `graph`.
pub(crate) fn broken_rules(
    model: &TemperModel,
    graph: &Graph,
    reach: &Reach,
    found: &HashMap<TemperModelState, u32>,
) -> Vec<Counterexample> {
    let bits = reach.bits;
    let broken: Vec<(String, Vec<String>, usize)> = model
        .liveness
        .iter()
        .filter_map(|live| {
            let LivenessKind::CanReach { from, targets } = &live.kind else {
                return None;
            };
            let checked = bits.of(model, from);
            let wanted = bits.of(model, targets);
            let in_from = |place: u32| {
                place != NO_PLACE
                    && (from.is_empty() || checked[place as usize / 64] & (1 << (place % 64)) != 0)
            };
            // The walk numbers states in the order it finds them, so the
            // first that breaks the rule is one of the nearest.
            (0..graph.len())
                .find(|&at| {
                    in_from(graph.place[at])
                        && !reach.past_bound(at)
                        && reach
                            .places(at)
                            .iter()
                            .zip(&wanted)
                            .all(|(r, t)| r & t == 0)
                })
                .map(|at| (live.name.clone(), targets.clone(), at))
        })
        .collect();
    if broken.is_empty() {
        return Vec::new();
    }

    let paths: Vec<Vec<(usize, Option<usize>)>> =
        broken.iter().map(|(_, _, at)| graph.path_to(*at)).collect();
    let wanted: BTreeSet<u32> = paths.iter().flatten().map(|(at, _)| *at as u32).collect();
    let states: BTreeMap<usize, &TemperModelState> = found
        .iter()
        .filter(|(_, at)| wanted.contains(at))
        .map(|(state, at)| (*at as usize, state))
        .collect();
    // The whole walk is complete. Replaying its evidence must neither spend
    // its remaining search budget nor stop when its deadline passes.
    let replay = TemperModel {
        limit: None,
        ..model.clone()
    };
    broken
        .into_iter()
        .zip(paths)
        .map(|((name, targets, _), path)| Counterexample {
            property: CAN_REACH.to_string(),
            violated: vec![name],
            trace: trace(&replay, &states, &path),
            never_reaches: targets,
        })
        .collect()
}

/// The states on `path` with the action taken from each: the transition's
/// name and the parameter values that step to the next state.
fn trace(
    model: &TemperModel,
    states: &BTreeMap<usize, &TemperModelState>,
    path: &[(usize, Option<usize>)],
) -> Vec<(TemperModelState, Option<TemperModelAction>)> {
    path.iter()
        .zip(path.iter().skip(1).map(Some).chain(std::iter::once(None)))
        .map(|(&(at, t), next)| {
            let state = states[&at];
            let action = t.map(|t| {
                let transition = &model.transitions[t];
                let next = states[&next.expect("a move leads somewhere").0];
                let params = successors(model, transition, state)
                    .into_iter()
                    .find(|(_, to)| to == next)
                    .map(|(params, _)| params)
                    .expect("the walk took this step");
                TemperModelAction {
                    name: transition.name.clone(),
                    target_state: transition.to_state.clone(),
                    params,
                }
            });
            (state.clone(), action)
        })
        .collect()
}
