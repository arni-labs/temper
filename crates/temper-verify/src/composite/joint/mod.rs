//! Invariants, over one entity or several.
//!
//! An `[[invariant]]` may read a related entity: its status
//! (`Type[id].status`) or a field (`Type[id].field`). One entity's model
//! cannot know those values, so the per-entity cascade reads them as unknown
//! and never fails on them. They are checked here, on the joint state of the
//! invariant's entity and every type it names, one record of each. An
//! invariant that names no other type is checked here too, on its entity
//! alone. Each model runs the way the runtime runs:
//!
//! - each move is made by whoever makes it in the runtime, and the app's
//!   Cedar decides it as them: an action someone outside may send (with the
//!   actions a `[[liveness]]` in scope `relies_on`), a reaction as its
//!   declared principal or its source's sender, and the kernel's own moves
//!   (state timers, module answers and failures, module calls lost to a
//!   restart). Without policies, anyone may send anything. Liveness reads
//!   the same senders (`senders.rs`);
//! - each record has an id, and a reference finds a record only when its id
//!   field holds that record's id; walks start from every way the records'
//!   ids can be equal or differ, so a design that works only when two ids
//!   happen to match is found;
//! - an action whose guard reads a related record (`Type[id].status`)
//!   reads it on the dispatch path and is written later by its record's
//!   actor, which checks only its own state; other moves can land in
//!   between;
//! - an entity reaction is a dispatch of its own, sent after its source
//!   commits with `args` copied from the source as committed, and taken by
//!   its target later, in any order with other dispatches;
//! - a reaction from a type the invariant does not name may come at any
//!   time, with any parameters: the model does not hold its source. It is a
//!   move from outside, sent as the reaction's declared principal (as any
//!   sender, without one), and Cedar decides it as that sender;
//! - string variables are tracked as symbols (equality only) when an
//!   invariant reads them, when a reference uses them as an id, or when
//!   their value reaches one that is read
//!   ([`crate::model::build_model_tracking`]);
//! - each entity is sliced to what the invariants depend on: a counter,
//!   boolean or list nothing read can be influenced by is dropped, and a
//!   guard that read it becomes a free choice. That only adds behaviour, so
//!   an invariant that holds here holds in the full model; a broken one
//!   comes with the path that breaks it, to read against the spec.
//!
//! Records disagree while a dispatch is on its way, so invariants are
//! checked in settled states: those with nothing in flight. The walk is
//! breadth first, so each broken invariant's path is a shortest one. A walk
//! that does not fit in the memory its [`Room`] gives it says so and is
//! never a pass.
//!
//! A call's answer is what the kernel dispatches: its trigger's `on_success`
//! with any values, or its `on_failure`
//! ([`call_may_answer`](temper_spec::automaton::call_may_answer)). The walk
//! takes each of them.
//!
//! Nor is a walk that checked nothing a pass. A rule written
//! `premise => conclusion` whose premise no walked state meets (no settled
//! one, for an invariant) holds without being tested, and proves nothing.
//!
//! The drop check (ADR-0150, [`dropped_reactions`]) walks the same model over
//! a whole trigger-graph component, unsliced, one record of each type that
//! every reference finds, with a move's dispatches landing before the next
//! move from outside, and names each reaction a target refuses.

mod check;
mod env;
mod liveness;
mod model;
mod pack;
mod proof;
mod replay;
mod room;
mod senders;
mod slice;
mod state;
mod vacuity;

pub use check::{JointCheck, JointVerdict, PROOF_TIME, check_joint};
pub use liveness::{
    JointLivenessResult, LivenessOutcome, LivenessProof, StuckCase, verify_joint_liveness,
};
pub use proof::{JointProof, joint_proofs};
pub use replay::{JointReplay, TraceMove, replay_joint_invariants};
pub use room::{Bound, Bytes, Progress, Room};
pub use vacuity::Premise;

use std::collections::BTreeSet;
use std::fmt;

use temper_authz::SendAnalysis;
use temper_spec::automaton::{ActionKind, Automaton, Invariant};
use temper_spec::predicate::Expr;

pub use model::DroppedReaction;
pub(crate) use model::dropped_reactions;
use model::{JointModel, Sending};
use proof::App;
pub use state::{Call, Dispatch, JointState};

/// What the joint check found for one invariant.
#[derive(Debug, Clone)]
pub struct JointInvariantResult {
    /// The entity type that declares the invariant.
    pub entity: String,
    /// The invariant's name.
    pub invariant: String,
    /// Its assertion, as written.
    pub assert: String,
    /// The entity types composed to check it.
    pub scope: Vec<String>,
    /// Distinct joint states walked.
    pub states: usize,
    /// When the walk held a move back because it would have put more than
    /// this many dispatches in flight: what it found holds only up to that.
    pub in_flight_bound: Option<usize>,
    /// Holds, broken (with a shortest path), or not fully checked.
    pub outcome: JointOutcome,
}

impl JointInvariantResult {
    /// Whether the invariant was checked over every reachable joint state and
    /// held in all of them.
    pub fn holds(&self) -> bool {
        matches!(self.outcome, JointOutcome::Holds)
    }
}

/// The outcome of checking one invariant on the joint model.
#[derive(Debug, Clone)]
pub enum JointOutcome {
    /// It held in every reachable joint state.
    Holds,
    /// A reachable joint state breaks it. `path` starts at the initial joint
    /// state; each later step names the move that led to it.
    Broken {
        /// The moves and the joint states they reach.
        path: Vec<JointStep>,
    },
    /// No settled state the walk met breaks it, but none meets its premise
    /// either, so that proves nothing.
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

/// The premise of a rule written `premise => conclusion`.
pub(crate) fn premise(rule: &Expr) -> Option<&Expr> {
    match rule {
        Expr::Implies(premise, _) => Some(premise),
        _ => None,
    }
}

/// One step of a counterexample path.
#[derive(Debug, Clone)]
pub struct JointStep {
    /// The move, `Entity.Action(params) -> Status`; `None` for the start.
    pub action: Option<String>,
    /// The joint state after it.
    pub state: String,
}

impl fmt::Display for JointInvariantResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = self.scope.join(" + ");
        match &self.outcome {
            JointOutcome::Holds => {
                write!(
                    f,
                    "{}.{} HOLDS over {} joint states ({scope}",
                    self.entity, self.invariant, self.states
                )?;
                match self.in_flight_bound {
                    Some(bound) => write!(f, "; with at most {bound} dispatches in flight)"),
                    None => f.write_str(")"),
                }
            }
            JointOutcome::Broken { path } => {
                writeln!(
                    f,
                    "{}.{} BROKEN after {} moves ({scope}, {} joint states walked): {}",
                    self.entity,
                    self.invariant,
                    path.len().saturating_sub(1),
                    self.states,
                    self.assert
                )?;
                for step in path {
                    match &step.action {
                        Some(action) => writeln!(f, "  {action}\n      {}", step.state)?,
                        None => writeln!(f, "  start\n      {}", step.state)?,
                    }
                }
                Ok(())
            }
            JointOutcome::Vacuous { premise } => write!(
                f,
                "{}.{} PROVES NOTHING: its premise `{premise}` holds in no settled state of the {} joint states walked ({scope})",
                self.entity, self.invariant, self.states
            ),
            JointOutcome::Incomplete { reason } => write!(
                f,
                "{}.{} NOT CHECKED ({scope}, {} joint states walked): {reason}",
                self.entity, self.invariant, self.states
            ),
        }
    }
}

/// The proof of one `[[invariant]]`, with all its walk reads: the invariant
/// and the joint model of its entity and the types it names, cut to what it
/// reads. Its [`JointProof::digest`] is of this value.
#[derive(Debug)]
pub struct InvariantProof {
    entity: String,
    invariant: Invariant,
    scope: BTreeSet<String>,
    /// The model, or why there is none: a type the invariant reads that the
    /// app does not declare.
    model: Result<JointModel, String>,
    app: App,
}

impl InvariantProof {
    /// Walk the model.
    pub fn walk(&self, bound: Bound<'_>) -> JointInvariantResult {
        let mut result = JointInvariantResult {
            entity: self.entity.clone(),
            invariant: self.invariant.name.clone(),
            assert: self.invariant.assert.to_string(),
            scope: self.scope.iter().cloned().collect(),
            states: 0,
            in_flight_bound: None,
            outcome: JointOutcome::Holds,
        };
        match &self.model {
            Ok(model) => {
                let mut walk = model.walk(&self.entity, &[&self.invariant.assert], bound);
                result.states = walk.states;
                result.in_flight_bound = walk.held_back.then_some(model::MAX_IN_FLIGHT);
                result.outcome = walk.outcomes.remove(0);
            }
            Err(reason) => {
                result.outcome = JointOutcome::Incomplete {
                    reason: reason.clone(),
                }
            }
        }
        result
    }

    /// The actions of the types in scope meant to come from outside (input
    /// and composite) that Cedar lets nobody outside send, as `Type.Action`.
    pub(super) fn unsent(&self) -> Vec<String> {
        let Ok(model) = &self.model else {
            return Vec::new();
        };
        let by_name = self.app.by_name();
        self.scope
            .iter()
            .flat_map(|entity| {
                by_name[entity.as_str()]
                    .actions
                    .iter()
                    .filter(|action| {
                        matches!(action.kind, ActionKind::Input | ActionKind::Composite)
                    })
                    .filter(|action| !model.sendable(entity, &action.name))
                    .map(move |action| format!("{entity}.{}", action.name))
            })
            .collect()
    }
}

/// The proofs of each `[[invariant]]` of `app` that `wanted` picks by its
/// type and name: one per invariant, on the
/// joint model of its entity and the types it names, with what a
/// `[[liveness]]` there relies on sent from outside by whoever Cedar lets,
/// since a rule must hold through those moves too. One that names no other
/// type is proved on its entity alone, by the same proof and walk, with the
/// kernel's moves and Cedar that the per-entity cascade does not read.
fn invariant_proofs(app: &App, wanted: impl Fn(&str, &str) -> bool) -> Vec<InvariantProof> {
    let by_name = app.by_name();
    let mut proofs = Vec::new();
    for (name, automaton) in &by_name {
        for invariant in automaton
            .invariants
            .iter()
            .filter(|invariant| wanted(name, &invariant.name))
        {
            let relies_on = |scope: &BTreeSet<String>| {
                scope
                    .iter()
                    .flat_map(|entity| {
                        by_name[entity.as_str()]
                            .liveness
                            .iter()
                            .flat_map(move |live| {
                                live.relies_on
                                    .iter()
                                    .map(move |action| (entity.clone(), action.clone()))
                            })
                    })
                    .collect()
            };
            let (scope, model) = joint_model(app, name, &invariant.assert, relies_on);
            proofs.push(InvariantProof {
                entity: name.to_string(),
                invariant: invariant.clone(),
                scope,
                model,
                app: app.clone(),
            });
        }
    }
    proofs
}

/// Walk every `[[invariant]]` in `automatons` on the joint model of its
/// entity and the types it names, with `policies`
/// deciding each move as its sender, as for liveness; without them every
/// sender may send everything.
pub fn verify_joint_invariants(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    bound: Bound<'_>,
) -> Vec<JointInvariantResult> {
    invariant_proofs(&App::new(automatons, policies), |_, _| true)
        .iter()
        .map(|proof| proof.walk(bound))
        .collect()
}

/// The types `owner`'s `rule` is walked over, its own and the types it
/// names, and their joint model, with the actions `relies_on` picks in that
/// scope sent from outside; or why there is none: a type it names that the
/// app does not declare.
fn joint_model(
    app: &App,
    owner: &str,
    rule: &Expr,
    relies_on: impl FnOnce(&BTreeSet<String>) -> BTreeSet<(String, String)>,
) -> (BTreeSet<String>, Result<JointModel, String>) {
    let by_name = app.by_name();
    let mut scope = BTreeSet::from([owner.to_string()]);
    let mut missing = Vec::new();
    for (entity_type, _) in rule.related_types() {
        if by_name.contains_key(entity_type.as_str()) {
            scope.insert(entity_type);
        } else {
            missing.push(entity_type);
        }
    }
    if !missing.is_empty() {
        let reason = format!(
            "it reads {}, which this app does not declare",
            missing.join(", ")
        );
        return (scope, Err(reason));
    }
    let sending = Sending {
        policies: app.policies(),
        kernel_moves: true,
        relies_on: relies_on(&scope),
    };
    let model = JointModel::new(&by_name, &scope, owner, &[rule], sending, true);
    (scope, Ok(model))
}

#[cfg(test)]
#[path = "joint_test.rs"]
mod tests;

#[cfg(test)]
#[path = "vacuous_test.rs"]
mod vacuous_tests;

#[cfg(test)]
#[path = "outside_test.rs"]
mod outside_tests;

#[cfg(test)]
#[path = "answers_test.rs"]
mod answers_tests;
