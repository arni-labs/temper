//! Whether a run meets a proved rule's premise.
//!
//! A proof by induction covers every run, and a rule written
//! `premise => conclusion` whose premise no run meets is proved without
//! saying anything. The walk does not pass such a rule, and neither does a
//! proof. The solver answers first ([`RuleProof::premise_excluded`]): when no
//! state the proof covers meets the premise, no run does. Otherwise a state
//! that meets it is looked for on the joint model, walked as the rule's own
//! walk would walk it, stopping at the first; it costs the states up to it,
//! never more than the rule's full walk.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, Invariant, Liveness};
use temper_spec::predicate::{Expr, Truth, eval};

use super::liveness::liveness_model;
use super::model::{JointModel, unwalked_reason};
use super::{invariants_model, premise};
use crate::Budget;
use crate::induct::RuleProof;

/// Whether a run meets a rule's premise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Premise {
    /// The rule has none.
    None,
    /// A walked state meets it: the joint states walked to find one, and
    /// how long building and walking the model took.
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

/// For each of `owner`'s proved invariants `rules`, whether a settled state
/// of their walk meets its premise.
pub(super) fn invariant_premises(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    rules: &[(&Invariant, &RuleProof)],
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> Vec<Premise> {
    let mut premises: Vec<Option<Premise>> = rules
        .iter()
        .map(|(rule, proof)| unwalked(&rule.assert, proof))
        .collect();
    let walked: Vec<&Expr> = rules
        .iter()
        .zip(&premises)
        .filter(|(_, known)| known.is_none())
        .map(|((rule, _), _)| &rule.assert)
        .collect();
    if !walked.is_empty() {
        let start = Instant::now();
        let mut found = match invariants_model(by_name, owner, &walked, policies) {
            Ok(model) => seek(&model, owner, &walked, true, budget, start),
            Err((_, missing)) => walked.iter().map(|r| undeclared(r, &missing)).collect(),
        }
        .into_iter();
        for known in premises.iter_mut().filter(|known| known.is_none()) {
            *known = found.next();
        }
    }
    premises
        .into_iter()
        .map(|known| known.expect("each walked rule has an answer"))
        .collect()
}

/// Whether a state of the walk of `owner`'s proved `live` rule, whose
/// condition is `goal`, meets the condition's premise.
pub(super) fn liveness_premise(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    (live, goal): (&Liveness, &Expr),
    proof: &RuleProof,
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> Premise {
    if let Some(known) = unwalked(goal, proof) {
        return known;
    }
    let start = Instant::now();
    match liveness_model(by_name, owner, live, goal, policies) {
        Ok(model) => seek(&model, owner, &[goal], false, budget, start).remove(0),
        Err((_, missing)) => undeclared(goal, &missing),
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
fn undeclared(rule: &Expr, missing: &[String]) -> Premise {
    match premise(rule) {
        Some(premise) => Premise::Unknown {
            premise: premise.to_string(),
            reason: format!(
                "it reads {}, which this app does not declare",
                missing.join(", ")
            ),
        },
        None => Premise::None,
    }
}

/// Walk `model` until a state meets each rule's premise, a settled one when
/// `settled`, as the walks read invariants and liveness. `start` is when
/// building the model began.
fn seek(
    model: &JointModel,
    owner: &str,
    rules: &[&Expr],
    settled: bool,
    budget: Budget,
    start: Instant,
) -> Vec<Premise> {
    let premises: Vec<Option<&Expr>> = rules.iter().map(|rule| premise(rule)).collect();
    let mut met: Vec<bool> = premises.iter().map(Option::is_none).collect();
    let explored = model.walk_with(budget, false, |_, state| {
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
    let unwalked = explored.unwalked();
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
                    None if !unwalked.is_empty() => Premise::Unknown {
                        premise,
                        reason: unwalked_reason(&unwalked),
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
