//! Whether a run meets a proved rule's premise.
//!
//! A proof by induction covers every run, and a rule written
//! `premise => conclusion` whose premise no run meets is proved without
//! saying anything. The walk does not pass such a rule, and neither does a
//! proof. The solver answers first ([`RuleProof::premise_excluded`]): when no
//! state the proof covers meets the premise, no run does. Otherwise a state
//! that meets it is looked for on the rule's own joint model, walked first
//! toward the statuses where the premise may hold and stopping at the first
//! state that meets it; it costs never more than the rule's full walk.

use std::time::{Duration, Instant};

use temper_spec::predicate::{Env, Expr, Truth, Val, eval};

use super::model::JointModel;
use super::premise;
use super::room::Bound;
use crate::induct::RuleProof;

/// Whether a run meets a rule's premise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Premise {
    /// The rule has none.
    None,
    /// A walked state meets it: the joint states walked to find one, and
    /// how long the walk took.
    Met { states: usize, took: Duration },
    /// The proof shows no state meets it.
    Excluded { premise: String },
    /// The walk saw every state, and none meets it.
    Never { premise: String, states: usize },
    /// The walk stopped before a state met it.
    Unknown { premise: String, reason: String },
}

impl Premise {
    /// Whether the rule says something: it has no premise, or a run meets it.
    pub fn met(&self) -> bool {
        matches!(self, Premise::None | Premise::Met { .. })
    }
}

/// Whether a settled state of the walk of `owner`'s proved invariant `rule`
/// meets its premise, `model` being that walk's model.
pub(super) fn premise_met(
    model: &Result<JointModel, String>,
    owner: &str,
    rule: &Expr,
    proof: &RuleProof,
    bound: Bound<'_>,
) -> Premise {
    seek_premise(model, owner, rule, proof, true, bound)
}

/// Whether a state of the walk of `owner`'s proved `eventually` rule, whose
/// condition is `goal`, meets the condition's premise, `model` being that
/// walk's model.
pub(super) fn premise_reached(
    model: &Result<JointModel, String>,
    owner: &str,
    goal: &Expr,
    proof: &RuleProof,
    bound: Bound<'_>,
) -> Premise {
    seek_premise(model, owner, goal, proof, false, bound)
}

fn seek_premise(
    model: &Result<JointModel, String>,
    owner: &str,
    rule: &Expr,
    proof: &RuleProof,
    settled: bool,
    bound: Bound<'_>,
) -> Premise {
    if let Some(known) = unwalked(rule, proof) {
        return known;
    }
    match model {
        Ok(model) => seek(model, owner, &[rule], settled, bound).remove(0),
        Err(reason) => undeclared(rule, reason),
    }
}

/// What is known of `rule`'s premise without a walk: that it has none, or
/// that its proof shows no state meets it.
fn unwalked(rule: &Expr, proof: &RuleProof) -> Option<Premise> {
    match premise(rule) {
        None => Some(Premise::None),
        Some(premise) if proof.premise_excluded => Some(Premise::Excluded {
            premise: premise.to_string(),
        }),
        Some(_) => None,
    }
}

/// A rule that reads types the app does not declare is not walked.
fn undeclared(rule: &Expr, reason: &str) -> Premise {
    match premise(rule) {
        Some(premise) => Premise::Unknown {
            premise: premise.to_string(),
            reason: reason.to_string(),
        },
        None => Premise::None,
    }
}

/// Walk `model` until a state meets each rule's premise, a settled one when
/// `settled`, as the walks read invariants and liveness.
fn seek(
    model: &JointModel,
    owner: &str,
    rules: &[&Expr],
    settled: bool,
    bound: Bound<'_>,
) -> Vec<Premise> {
    let start = Instant::now();
    let premises: Vec<Option<&Expr>> = rules.iter().map(|rule| premise(rule)).collect();
    let mut met: Vec<bool> = premises.iter().map(Option::is_none).collect();
    let may_meet = |status: &str| {
        let status = Status(status);
        premises
            .iter()
            .flatten()
            .any(|p| eval(p, &status) != Truth::False)
    };
    let explored = model.walk_toward(bound, (owner, may_meet), |_, state| {
        if !settled || state.settled() {
            let env = model.env(owner, state);
            for (met, premise) in met.iter_mut().zip(&premises) {
                if let (false, Some(premise)) = (*met, premise) {
                    *met = eval(premise, &env) != Truth::False;
                }
            }
        }
        met.iter().all(|met| *met)
    });
    let took = start.elapsed();
    premises
        .iter()
        .zip(&met)
        .map(|(premise, met)| match (premise, met) {
            (None, _) => Premise::None,
            (_, true) => Premise::Met {
                states: explored.len(),
                took,
            },
            (Some(premise), false) => {
                let premise = premise.to_string();
                match &explored.stopped {
                    Some(reason) => Premise::Unknown {
                        premise,
                        reason: reason.clone(),
                    },
                    None => Premise::Never {
                        premise,
                        states: explored.len(),
                    },
                }
            }
        })
        .collect()
}

/// A record whose status alone is known: a premise false there is false
/// whatever its fields hold.
struct Status<'a>(&'a str);

impl Env for Status<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(self.0)
    }

    fn var(&self, _: &str) -> Val<'_> {
        Val::Unknown
    }

    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}
