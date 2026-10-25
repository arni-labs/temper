//! Conditions records reach by themselves.
//!
//! A `[[liveness]]` with `eventually` says the system, left alone, brings
//! the records to a state where the condition holds. From every reachable
//! joint state, the moves the system makes (state timers, module answers,
//! the kernel's dispatches and reactions, and the actions outside code is
//! relied on to send while the condition is false) can lead to a state with
//! nothing in flight where it holds. Every reachable state is a start,
//! because a person's move can lead anywhere and nobody is assumed to come
//! back and fix it.
//!
//! "Can lead" is what is checked: a path of system moves exists. In a finite
//! model that is what "eventually, as long as each move the system can keep
//! making is made at some point" comes to, so a module that fails again and
//! again does not count against it, while a state no system move leaves
//! does.
//!
//! Three choices keep a pass sound. The walk offers an outside move wherever
//! Cedar may let someone outside the kernel send it, which only adds starting
//! states. Each entity is cut down to what the condition depends on, which
//! only adds behaviour too. And a system move counts only when the model
//! knows it can happen: one that took a guard the model could not decide
//! (one that reads a related record not yet read, or a variable the cut
//! dropped) is not used.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Truth, eval};

use super::model::{self, JointModel, Sending};
use super::{JointStep, path_to};
use crate::Budget;

/// What the joint check found for one `eventually` property.
#[derive(Debug, Clone)]
pub struct JointLivenessResult {
    /// The entity type that declares it.
    pub entity: String,
    /// The property's name.
    pub name: String,
    /// Its condition, as written.
    pub eventually: String,
    /// The actions it relies on outside code to send.
    pub relies_on: Vec<String>,
    /// The entity types composed to check it.
    pub scope: Vec<String>,
    /// Distinct joint states walked.
    pub states: usize,
    /// When the walk held a move back because it would have put more than
    /// this many dispatches and module calls in flight.
    pub in_flight_bound: Option<usize>,
    /// Settles, stuck (with a shortest path to a state it cannot leave), or
    /// not fully checked.
    pub outcome: LivenessOutcome,
}

impl JointLivenessResult {
    /// Whether every reachable state can reach the condition.
    pub fn holds(&self) -> bool {
        matches!(self.outcome, LivenessOutcome::Holds)
    }
}

/// The outcome of checking one `eventually` property.
#[derive(Debug, Clone)]
pub enum LivenessOutcome {
    /// From every reachable state, the system's moves can reach the
    /// condition with nothing in flight.
    Holds,
    /// Reachable states from which no path of system moves reaches it.
    Stuck {
        /// How many walked states cannot reach it.
        stuck: usize,
        /// Each way it comes to rest short of the condition: one per set of
        /// record statuses among the stuck states with nothing in flight.
        cases: Vec<StuckCase>,
    },
    /// The walk stopped before every state was seen, or could not start.
    Incomplete {
        /// Why.
        reason: String,
    },
}

/// A stuck state with nothing in flight, and how the walk reached it.
#[derive(Debug, Clone)]
pub struct StuckCase {
    /// A shortest path from the start to it.
    pub path: Vec<JointStep>,
    /// The system's moves from it; none of them leads to the condition.
    pub moves: Vec<String>,
}

impl fmt::Display for StuckCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.path {
            match &step.action {
                Some(action) => writeln!(f, "    {action}\n        {}", step.state)?,
                None => writeln!(f, "    start\n        {}", step.state)?,
            }
        }
        if self.moves.is_empty() {
            writeln!(f, "    the system makes no move from there")
        } else {
            writeln!(
                f,
                "    the system's moves from there, none of which gets there:"
            )?;
            self.moves.iter().try_for_each(|m| writeln!(f, "      {m}"))
        }
    }
}

impl fmt::Display for JointLivenessResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = self.scope.join(" + ");
        let relying = if self.relies_on.is_empty() {
            String::new()
        } else {
            format!(
                "; relying on {} while it is false",
                self.relies_on.join(", ")
            )
        };
        let bound = self.in_flight_bound.map_or(String::new(), |bound| {
            format!("; with at most {bound} dispatches and calls in flight")
        });
        match &self.outcome {
            LivenessOutcome::Holds => write!(
                f,
                "{}.{} SETTLES: from each of {} joint states the system's own moves reach `{}` ({scope}{relying}{bound})",
                self.entity, self.name, self.states, self.eventually
            ),
            LivenessOutcome::Stuck { stuck, cases } => {
                writeln!(
                    f,
                    "{}.{} NEVER SETTLES: from {stuck} of {} joint states no path of the system's moves leads to `{}` ({scope}{relying}{bound}). It comes to rest short of it in {} ways, each with a shortest path:",
                    self.entity,
                    self.name,
                    self.states,
                    self.eventually,
                    cases.len()
                )?;
                for (number, case) in cases.iter().enumerate() {
                    writeln!(
                        f,
                        "  {}. after {} moves",
                        number + 1,
                        case.path.len().saturating_sub(1)
                    )?;
                    write!(f, "{case}")?;
                }
                Ok(())
            }
            LivenessOutcome::Incomplete { reason } => write!(
                f,
                "{}.{} NOT CHECKED ({scope}, {} joint states walked): {reason}",
                self.entity, self.name, self.states
            ),
        }
    }
}

/// Check every `[[liveness]]` with `eventually` in `automatons` on the joint
/// model of its entity and the types its condition names. `policies` decide
/// each move as its sender; without them every sender may send everything.
pub fn verify_joint_liveness(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> Vec<JointLivenessResult> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let mut results = Vec::new();
    for (name, automaton) in &by_name {
        for live in &automaton.liveness {
            let Some(goal) = &live.eventually else {
                continue;
            };
            let mut scope = BTreeSet::from([name.to_string()]);
            let mut missing = Vec::new();
            for (entity_type, _) in goal.related_types() {
                if by_name.contains_key(entity_type.as_str()) {
                    scope.insert(entity_type);
                } else {
                    missing.push(entity_type);
                }
            }
            let mut result = JointLivenessResult {
                entity: name.to_string(),
                name: live.name.clone(),
                eventually: goal.to_string(),
                relies_on: live.relies_on.clone(),
                scope: scope.iter().cloned().collect(),
                states: 0,
                in_flight_bound: None,
                outcome: LivenessOutcome::Holds,
            };
            if !missing.is_empty() {
                result.outcome = LivenessOutcome::Incomplete {
                    reason: format!(
                        "it reads {}, which this app does not declare",
                        missing.join(", ")
                    ),
                };
                results.push(result);
                continue;
            }
            let sending = Sending {
                policies,
                kernel_moves: true,
                relies_on: live
                    .relies_on
                    .iter()
                    .map(|action| (name.to_string(), action.clone()))
                    .collect(),
            };
            let model = JointModel::new(&by_name, scope, name, &[goal], sending, true);
            let explored = model.walk_with(budget, true, |_, _| false);
            result.states = explored.nodes.len();
            result.in_flight_bound = explored.held_back.then_some(model::MAX_IN_FLIGHT);
            result.outcome = match &explored.stopped {
                Some(reason) => LivenessOutcome::Incomplete {
                    reason: reason.clone(),
                },
                None => settle(&model, name, goal, &explored),
            };
            results.push(result);
        }
    }
    results
}

/// Whether every walked state reaches `goal` through the system's moves,
/// and if not, each way the stuck states come to rest: the first stuck state
/// with nothing in flight (in walk order, so with a shortest path) for each
/// set of record statuses.
fn settle(
    model: &JointModel,
    owner: &str,
    goal: &temper_spec::predicate::Expr,
    explored: &model::Explored,
) -> LivenessOutcome {
    let count = explored.nodes.len();
    let mut good = vec![false; count];
    let mut failing = vec![false; count];
    let mut queue = VecDeque::new();
    for (index, (state, _)) in explored.nodes.iter().enumerate() {
        let truth = eval(goal, &model.env(owner, state));
        failing[index] = truth != Truth::True;
        if truth == Truth::True && state.settled() {
            good[index] = true;
            queue.push_back(index);
        }
    }
    let mut back: Vec<Vec<u32>> = vec![Vec::new(); count];
    for edge in &explored.edges {
        let usable =
            edge.certain && (edge.by_system || (edge.relied && failing[edge.from as usize]));
        if usable {
            back[edge.to as usize].push(edge.from);
        }
    }
    while let Some(index) = queue.pop_front() {
        for &from in &back[index] {
            if !good[from as usize] {
                good[from as usize] = true;
                queue.push_back(from as usize);
            }
        }
    }
    let stuck: Vec<usize> = (0..count).filter(|&index| !good[index]).collect();
    let Some(&first) = stuck.first() else {
        return LivenessOutcome::Holds;
    };
    let mut seen = BTreeSet::new();
    let mut resting: Vec<usize> = stuck
        .iter()
        .copied()
        .filter(|&index| {
            let state = &explored.nodes[index].0;
            let statuses: Vec<&str> = state.records.values().map(|r| r.status.as_str()).collect();
            state.settled() && seen.insert(statuses)
        })
        .collect();
    if resting.is_empty() {
        resting.push(first);
    }
    let cases = resting
        .into_iter()
        .map(|index| {
            let state = &explored.nodes[index].0;
            let mut moves = BTreeSet::new();
            for mv in model.moves(state) {
                let relied = failing[index] && model.relies(&mv);
                if !mv.by_system() && !relied {
                    continue;
                }
                for next in model.next(state, &mv).0 {
                    moves.insert(next.label);
                }
            }
            StuckCase {
                path: path_to(&explored.nodes, index),
                moves: moves.into_iter().collect(),
            }
        })
        .collect();
    LivenessOutcome::Stuck {
        stuck: stuck.len(),
        cases,
    }
}

#[cfg(test)]
#[path = "liveness_test.rs"]
mod tests;
