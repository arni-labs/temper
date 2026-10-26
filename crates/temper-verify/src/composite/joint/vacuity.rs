//! Whether a run meets a proved rule's premise.
//!
//! A proof by induction covers every run, and a rule written
//! `premise => conclusion` whose premise no run meets is proved without
//! saying anything. The walk does not pass such a rule, and neither does a
//! proof: the joint model is walked as the rule's own walk would walk it,
//! and stops once a state meets the premise. A rule that says something
//! costs the states up to its first premise, never more than its full walk.

use std::collections::BTreeMap;

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, Invariant, Liveness};
use temper_spec::predicate::{Expr, Truth, eval};

use super::liveness::liveness_model;
use super::model::{JointModel, unwalked_reason};
use super::{invariants_model, premise};
use crate::Budget;

/// Whether a run meets a rule's premise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Premise {
    /// The rule has none, or a walked state meets it.
    Met,
    /// The walk saw every state, and none meets it.
    Never { premise: String, states: usize },
    /// The walk stopped before a state met it.
    Unknown { premise: String, reason: String },
}

/// For each of `owner`'s invariants `rules`, whether a settled state of
/// their walk meets its premise.
pub(super) fn invariant_premises(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    rules: &[&Invariant],
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> Vec<Premise> {
    let exprs: Vec<&Expr> = rules.iter().map(|rule| &rule.assert).collect();
    match invariants_model(by_name, owner, &exprs, policies) {
        Ok(model) => seek(&model, owner, &exprs, true, budget),
        Err((_, missing)) => exprs
            .iter()
            .map(|rule| undeclared(rule, &missing))
            .collect(),
    }
}

/// Whether a state of the walk of `owner`'s `live` rule, whose condition is
/// `goal`, meets the condition's premise.
pub(super) fn liveness_premise(
    by_name: &BTreeMap<&str, &Automaton>,
    owner: &str,
    live: &Liveness,
    goal: &Expr,
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> Premise {
    match liveness_model(by_name, owner, live, goal, policies) {
        Ok(model) => seek(&model, owner, &[goal], false, budget).remove(0),
        Err((_, missing)) => undeclared(goal, &missing),
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
        None => Premise::Met,
    }
}

/// Walk `model` until a state meets each rule's premise, a settled one when
/// `settled`, as the walks read invariants and liveness.
fn seek(
    model: &JointModel,
    owner: &str,
    rules: &[&Expr],
    settled: bool,
    budget: Budget,
) -> Vec<Premise> {
    let premises: Vec<Option<&Expr>> = rules.iter().map(|rule| premise(rule)).collect();
    let mut met: Vec<bool> = premises.iter().map(Option::is_none).collect();
    if met.iter().all(|met| *met) {
        return vec![Premise::Met; rules.len()];
    }
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
    let unwalked = explored.unwalked();
    premises
        .iter()
        .zip(&met)
        .map(|(premise, met)| match (premise, met) {
            (None, _) | (_, true) => Premise::Met,
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
