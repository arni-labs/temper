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
//! states. A reaction from a type the condition does not name is offered the
//! same way, as its sender, and is not counted on: its source may never move. Each entity is cut down to what the condition depends on, which
//! only adds behaviour too. And the model counts on a system move only for
//! what it knows of it. Where it cannot decide a guard, a constraint or
//! Cedar, the move leads to one of the ways it can go (taken or refused),
//! and it counts only when every one of them gets there. Where the model
//! does not know all the ways (it filled in a value nobody sent, or more
//! than one transition may be taken), it does not count on the move at all.
//! A sender that picks its own parameters (a module's answer, or a request
//! outside code is relied on to send) is taken to send ones that pass,
//! unless a parameter must equal a field the model does not track.
//!
//! A count a reaction carries, which the receiver takes only when it is
//! greater than its own, is kept as a sequence number: the newest, or
//! behind it (`model/order.rs`). A callee chooses its returned parameters
//! independently of the fields held when its call started.
//!
//! A pass that checked nothing is not one: a condition written
//! `premise => conclusion` whose premise no walked state meets proves nothing.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Truth, eval};

use super::lists::Lists;
use super::model::{self, JointModel};
use super::proof::App;
use super::room::{Bound, MB};
use super::{JointStep, joint_model};

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
    /// Settles, stuck (with a shortest path to a state it cannot leave),
    /// proves nothing, or not fully checked.
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
    /// Every walked state reaches the condition, but none meets its premise,
    /// so that proves nothing.
    Vacuous {
        /// The premise, as written.
        premise: String,
    },
    /// The walk stopped before every state was seen, never took an answer
    /// a module may give, or could not start.
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
            LivenessOutcome::Vacuous { premise } => write!(
                f,
                "{}.{} CHECKS NOTHING AS WRITTEN: its premise `{premise}` holds in none of the {} joint states walked ({scope}{relying}{bound})",
                self.entity, self.name, self.states
            ),
            LivenessOutcome::Incomplete { reason } => write!(
                f,
                "{}.{} NOT CHECKED ({scope}, {} joint states walked): {reason}",
                self.entity, self.name, self.states
            ),
        }
    }
}

/// The proof of one `[[liveness]]` with `eventually`, with all its walk
/// reads: the property and the joint model of its entity and the types its
/// condition names, cut to what it reads. Its
/// [`JointProof::digest`](super::JointProof::digest) is of this value.
#[derive(Debug)]
pub struct LivenessProof {
    pub(super) entity: String,
    pub(super) name: String,
    pub(super) goal: Expr,
    relies_on: Vec<String>,
    /// Whether it carries a measure, without which induction does not try it.
    pub(super) measured: bool,
    scope: BTreeSet<String>,
    /// The model, or why there is none: a type the condition reads that the
    /// app does not declare.
    pub(super) model: Result<JointModel, String>,
    pub(super) app: App,
}

impl LivenessProof {
    /// Walk the model and decide whether the records reach the condition.
    pub fn walk(&self, bound: Bound<'_>) -> JointLivenessResult {
        let mut result = JointLivenessResult {
            entity: self.entity.clone(),
            name: self.name.clone(),
            eventually: self.goal.to_string(),
            relies_on: self.relies_on.clone(),
            scope: self.scope.iter().cloned().collect(),
            states: 0,
            in_flight_bound: None,
            outcome: LivenessOutcome::Holds,
        };
        let model = match &self.model {
            Ok(model) => model,
            Err(reason) => {
                result.outcome = LivenessOutcome::Incomplete {
                    reason: reason.clone(),
                };
                return result;
            }
        };
        let mut explored = model.walk_with(bound, true, |_, _| false);
        result.states = explored.len();
        result.in_flight_bound = explored.held_back.then_some(model::MAX_IN_FLIGHT);
        let settling = settle_bytes(&explored);
        result.outcome = match &explored.stopped {
            Some(reason) => LivenessOutcome::Incomplete {
                reason: reason.clone(),
            },
            None => match explored.held.add(settling) {
                Ok(()) => match settle(model, &self.entity, &self.goal, &explored) {
                    (LivenessOutcome::Holds, false) => LivenessOutcome::Vacuous {
                        premise: super::premise(&self.goal)
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                    },
                    (outcome, _) => outcome,
                },
                Err(reason) => LivenessOutcome::Incomplete {
                    reason: format!(
                        "deciding the walk does not fit in the memory it may hold: {reason} \
                         (it held {} MB at {} states and needs {} MB more)",
                        explored.held.bytes / MB,
                        explored.len(),
                        settling / MB
                    ),
                },
            },
        };
        result
    }
}

/// The proofs of every `[[liveness]]` with `eventually` of `app`: one per
/// property, on the joint model of its entity and the types its condition
/// names, with the actions it relies on sent from outside.
pub(super) fn liveness_proofs(app: &App) -> Vec<LivenessProof> {
    let mut proofs = Vec::new();
    for (name, automaton) in &app.by_name() {
        for live in &automaton.liveness {
            let Some(goal) = &live.eventually else {
                continue;
            };
            let relies_on = |_: &BTreeSet<String>| {
                live.relies_on
                    .iter()
                    .map(|action| (name.to_string(), action.clone()))
                    .collect()
            };
            let (scope, model) = joint_model(app, name, goal, relies_on);
            proofs.push(LivenessProof {
                entity: name.to_string(),
                name: live.name.clone(),
                goal: goal.clone(),
                relies_on: live.relies_on.clone(),
                measured: !live.measure.is_empty(),
                scope,
                model,
                app: app.clone(),
            });
        }
    }
    proofs
}

/// Walk every `[[liveness]]` with `eventually` in `automatons` on the joint
/// model of its entity and the types its condition names, with `policies`
/// deciding each move as its sender; without them every sender may send
/// everything.
pub fn verify_joint_liveness(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    bound: Bound<'_>,
) -> Vec<JointLivenessResult> {
    liveness_proofs(&App::new(automatons, policies))
        .iter()
        .map(|proof| proof.walk(bound))
        .collect()
}

/// Whether every walked state reaches `goal` through the system's moves,
/// and if not, each way the stuck states come to rest: the first stuck state
/// with nothing in flight (in walk order, so with a shortest path) for each
/// set of record statuses. With it, whether a walked state meets the goal's
/// premise (always, when it has none).
///
/// A state reaches it when it holds there with nothing in flight, or when a
/// move the system makes there either can lead to a state that reaches it,
/// or always ends in one of a few states that all do (a guard the model
/// cannot decide, taken or refused).
fn settle(
    model: &JointModel,
    owner: &str,
    goal: &temper_spec::predicate::Expr,
    explored: &model::Explored,
) -> (LivenessOutcome, bool) {
    let count = explored.len();
    let mut good = vec![false; count];
    let mut failing = vec![false; count];
    let mut queue = VecDeque::new();
    let premise = super::premise(goal);
    let mut met = premise.is_none();
    for index in 0..count {
        let state = explored.state(index);
        let env = model.env(owner, &state);
        let truth = eval(goal, &env);
        if let (false, Some(premise)) = (met, premise) {
            met = eval(premise, &env) != Truth::False;
        }
        failing[index] = truth != Truth::True;
        if truth == Truth::True && state.settled() {
            good[index] = true;
            queue.push_back(index);
        }
    }
    let usable: Vec<bool> = explored
        .made
        .iter()
        .map(|made| made.by_system || (made.relied && failing[made.from as usize]))
        .collect();
    // For each state, the states with a sure move to it.
    let sure_from = Lists::build(count, |put| {
        for edge in &explored.edges {
            if edge.sure && usable[edge.made as usize] {
                put(edge.to, explored.made[edge.made as usize].from);
            }
        }
    });
    // For each state, the covered moves that may end in it. A move's edges
    // are walked one after another, so the states it ends in are told apart
    // with a short list.
    let covering = Lists::build(count, |put| {
        let mut ends: (u32, Vec<u32>) = (u32::MAX, Vec::new());
        for edge in &explored.edges {
            if !usable[edge.made as usize] || !explored.made[edge.made as usize].covered {
                continue;
            }
            if ends.0 != edge.made {
                ends = (edge.made, Vec::new());
            }
            if !ends.1.contains(&edge.to) {
                ends.1.push(edge.to);
                put(edge.to, edge.made);
            }
        }
    });
    // For each covered move, how many of the states it may end in do not
    // reach the goal yet.
    let mut open = vec![0u32; explored.made.len()];
    for &made in &covering.items {
        open[made as usize] += 1;
    }
    while let Some(index) = queue.pop_front() {
        let mut reached: Vec<u32> = sure_from.get(index).to_vec();
        for &made in covering.get(index) {
            open[made as usize] -= 1;
            if open[made as usize] == 0 {
                reached.push(explored.made[made as usize].from);
            }
        }
        for from in reached {
            if !good[from as usize] {
                good[from as usize] = true;
                queue.push_back(from as usize);
            }
        }
    }
    let stuck: Vec<usize> = (0..count).filter(|&index| !good[index]).collect();
    let Some(&first) = stuck.first() else {
        return (LivenessOutcome::Holds, met);
    };
    let mut seen = BTreeSet::new();
    let mut resting: Vec<usize> = stuck
        .iter()
        .copied()
        .filter(|&index| {
            let state = explored.state(index);
            let statuses: Vec<String> = state.records.values().map(|r| r.status.clone()).collect();
            state.settled() && seen.insert(statuses)
        })
        .collect();
    if resting.is_empty() {
        resting.push(first);
    }
    let cases = resting
        .into_iter()
        .map(|index| {
            let state = &explored.state(index);
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
                path: explored.path(index),
                moves: moves.into_iter().collect(),
            }
        })
        .collect();
    (
        LivenessOutcome::Stuck {
            stuck: stuck.len(),
            cases,
        },
        met,
    )
}

/// The bytes [`settle`] holds for `explored`: per state, its marks, its
/// place in the queue and the stuck list, and where its two lists of moves
/// in start; per move, its mark and count; per edge, its place in each of
/// those lists.
fn settle_bytes(explored: &model::Explored<'_>) -> usize {
    let per_state = 2 * size_of::<bool>() + 4 * size_of::<usize>();
    let per_move = size_of::<bool>() + size_of::<u32>();
    explored.len() * per_state
        + explored.made.len() * per_move
        + explored.edges.len() * 2 * size_of::<u32>()
}

#[cfg(test)]
#[path = "liveness_test.rs"]
mod tests;

#[cfg(test)]
#[path = "liveness_state_test.rs"]
mod state_tests;
