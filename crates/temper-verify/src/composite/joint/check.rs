//! The joint check an install and `temper verify` run.
//!
//! Each rule over several records is proved by induction first
//! ([`crate::induct`]), for every run rather than the runs a bounded walk
//! reaches: with the lemmas written beside it, or with none, the rule at
//! every state and then the prover's own guesses, which a verdict lists so
//! they can be written beside the rule. An `eventually` rule is tried when
//! it carries its measure, which nothing guesses. A rule whose proof does not
//! hold is walked as before and keeps the walk's verdict, with why the proof
//! did not hold.
//!
//! A proved rule is walked only as far as a state that meets its premise
//! ([`vacuity`](super::vacuity)), since a rule whose premise no run meets
//! says nothing. The proofs get the budget's time between them, and each walk
//! the budget it had before.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Instant;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::Expr;

use super::liveness::{JointLivenessResult, LivenessOutcome, verify_joint_liveness};
use super::vacuity::{Premise, invariant_premises, liveness_premise};
use super::{JointInvariantResult, JointOutcome, verify_joint_invariants};
use crate::Budget;
use crate::induct::{Hints, RuleProof, prove_some};

/// What the joint check found, rule by rule.
#[derive(Debug, Clone, Default)]
pub struct JointCheck {
    /// Each `[[invariant]]` that reads a related record.
    pub invariants: Vec<JointVerdict>,
    /// Each `[[liveness]]` with `eventually`.
    pub liveness: Vec<JointVerdict>,
}

/// One rule's verdict.
#[derive(Debug, Clone)]
pub enum JointVerdict {
    /// Proved by induction, for every run, and whether a run meets its
    /// premise.
    Proved(RuleProof, Premise),
    /// Walked, with the proof that was tried and did not hold.
    Invariant(JointInvariantResult, Option<RuleProof>),
    /// Walked, with the proof that was tried and did not hold.
    Liveness(JointLivenessResult, Option<RuleProof>),
}

impl JointVerdict {
    /// Whether the rule holds in every run, as proved or walked in full.
    pub fn holds(&self) -> bool {
        match self {
            JointVerdict::Proved(_, premise) => *premise == Premise::Met,
            JointVerdict::Invariant(walked, _) => walked.holds(),
            JointVerdict::Liveness(walked, _) => walked.holds(),
        }
    }

    /// The rule, as `Type.name`.
    pub fn rule(&self) -> String {
        match self {
            JointVerdict::Proved(proof, _) => format!("{}.{}", proof.owner, proof.rule),
            JointVerdict::Invariant(walked, _) => format!("{}.{}", walked.entity, walked.invariant),
            JointVerdict::Liveness(walked, _) => format!("{}.{}", walked.entity, walked.name),
        }
    }

    /// The verdict in a word.
    pub fn tag(&self) -> &'static str {
        match self {
            JointVerdict::Proved(_, Premise::Met) => "PROVED",
            JointVerdict::Proved(_, Premise::Never { .. }) => "VACUOUS",
            JointVerdict::Proved(_, Premise::Unknown { .. }) => "INCOMPLETE",
            JointVerdict::Invariant(walked, _) => match walked.outcome {
                JointOutcome::Holds => "PASS",
                JointOutcome::Broken { .. } => "FAIL",
                JointOutcome::Vacuous { .. } => "VACUOUS",
                JointOutcome::Incomplete { .. } => "INCOMPLETE",
            },
            JointVerdict::Liveness(walked, _) => match walked.outcome {
                LivenessOutcome::Holds => "PASS",
                LivenessOutcome::Stuck { .. } => "FAIL",
                LivenessOutcome::Vacuous { .. } => "VACUOUS",
                LivenessOutcome::Incomplete { .. } => "INCOMPLETE",
            },
        }
    }
}

impl fmt::Display for JointVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (walked, tried) = match self {
            JointVerdict::Proved(proof, premise) => return proved(f, &self.rule(), proof, premise),
            JointVerdict::Invariant(walked, tried) => (walked.to_string(), tried),
            JointVerdict::Liveness(walked, tried) => (walked.to_string(), tried),
        };
        f.write_str(walked.trim_end())?;
        match tried.as_ref().and_then(RuleProof::shortfall) {
            Some(why) => write!(f, "\n  not proved by induction: {why}"),
            None => Ok(()),
        }
    }
}

/// A proved rule's verdict in words, with the guesses the proof kept.
fn proved(
    f: &mut fmt::Formatter<'_>,
    rule: &str,
    proof: &RuleProof,
    premise: &Premise,
) -> fmt::Result {
    let scope = proof.scope.iter().cloned().collect::<Vec<_>>().join(" + ");
    match premise {
        Premise::Met => write!(
            f,
            "{rule} PROVED for every run by induction ({scope}; {} obligations in {} ms)",
            proof.obligations.len(),
            proof.took.as_millis()
        )?,
        Premise::Never { premise, states } => write!(
            f,
            "{rule} PROVES NOTHING: proved by induction, but its premise `{premise}` holds in none of the {states} joint states walked ({scope})"
        )?,
        Premise::Unknown { premise, reason } => write!(
            f,
            "{rule} NOT CHECKED: proved by induction, but no walked state met its premise `{premise}` ({scope}): {reason}"
        )?,
    }
    if !proof.found.is_empty() {
        f.write_str("\n  with the prover's guesses, which can be written beside the rule:")?;
        for lemma in &proof.found {
            write!(f, "\n    {}: {}", lemma.name, lemma.assert)?;
        }
    }
    Ok(())
}

/// Check the rules over several records of `automatons`, as
/// [`verify_joint_invariants`] and [`verify_joint_liveness`] pick them,
/// with `policies` deciding each move as its sender: by induction first, and
/// by the walk where the proof does not hold.
pub fn check_joint(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> JointCheck {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    // A rule that reads a type the app does not declare is reported by the
    // walk, which says so.
    let all_declared = |expr: &Expr| {
        expr.related_types()
            .iter()
            .all(|(entity_type, _)| by_name.contains_key(entity_type.as_str()))
    };
    let mut invariants = BTreeSet::new();
    let mut liveness = BTreeSet::new();
    for automaton in automatons {
        let owner = &automaton.automaton.name;
        for invariant in &automaton.invariants {
            // MUTANT: only a rule whose spec gives lemmas is tried.
            if !invariant.assert.related_types().is_empty()
                && all_declared(&invariant.assert)
                && !invariant.lemma.is_empty()
            {
                invariants.insert((owner.clone(), invariant.name.clone()));
            }
        }
        for live in &automaton.liveness {
            if let Some(goal) = &live.eventually
                && !live.measure.is_empty()
                && all_declared(goal)
            {
                liveness.insert((owner.clone(), live.name.clone()));
            }
        }
    }
    let proofs = if invariants.is_empty() && liveness.is_empty() {
        Vec::new()
    } else {
        prove_some(
            automatons,
            policies,
            &Hints::default(),
            |owner, rule| {
                let key = (owner.to_string(), rule.to_string());
                invariants.contains(&key) || liveness.contains(&key)
            },
            Some(Instant::now() + budget.time),
        )
    };
    let mut check = JointCheck::default();
    let mut proved = BTreeSet::new();
    let mut tried = BTreeMap::new();
    // Proved invariants by their type, which walk together for premises.
    let mut owned: BTreeMap<String, Vec<RuleProof>> = BTreeMap::new();
    for proof in proofs {
        let key = (proof.owner.clone(), proof.rule.clone());
        if !proof.proved() {
            tried.insert(key, proof);
            continue;
        }
        proved.insert(key.clone());
        if invariants.contains(&key) {
            owned.entry(proof.owner.clone()).or_default().push(proof);
            continue;
        }
        let automaton = by_name[proof.owner.as_str()];
        let premise = match automaton.liveness.iter().find(|l| l.name == proof.rule) {
            Some(
                live @ temper_spec::automaton::Liveness {
                    eventually: Some(goal),
                    ..
                },
            ) => liveness_premise(&by_name, &proof.owner, live, goal, policies, budget),
            _ => Premise::Met,
        };
        check.liveness.push(JointVerdict::Proved(proof, premise));
    }
    for (owner, proofs) in owned {
        let automaton = by_name[owner.as_str()];
        let rules: Vec<_> = proofs
            .iter()
            .filter_map(|p| automaton.invariants.iter().find(|i| i.name == p.rule))
            .collect();
        let premises = invariant_premises(&by_name, &owner, &rules, policies, budget);
        for (proof, premise) in proofs.into_iter().zip(premises) {
            check.invariants.push(JointVerdict::Proved(proof, premise));
        }
    }
    let unproved =
        |owner: &str, rule: &str| !proved.contains(&(owner.to_string(), rule.to_string()));
    // Each walk sees the rules left. A proved liveness rule stays for the
    // invariants' walk: what it relies on is still sent from outside.
    let for_invariants = edited(automatons, |a| {
        let owner = a.automaton.name.clone();
        a.invariants.retain(|i| unproved(&owner, &i.name));
    });
    let refs: Vec<&Automaton> = for_invariants.iter().collect();
    for walked in verify_joint_invariants(&refs, policies, budget) {
        let key = (walked.entity.clone(), walked.invariant.clone());
        let proof = tried.remove(&key);
        check
            .invariants
            .push(JointVerdict::Invariant(walked, proof));
    }
    let for_liveness = edited(automatons, |a| {
        let owner = a.automaton.name.clone();
        a.liveness.retain(|l| unproved(&owner, &l.name));
    });
    let refs: Vec<&Automaton> = for_liveness.iter().collect();
    for walked in verify_joint_liveness(&refs, policies, budget) {
        let key = (walked.entity.clone(), walked.name.clone());
        let proof = tried.remove(&key);
        check.liveness.push(JointVerdict::Liveness(walked, proof));
    }
    check
}

/// Copies of `automatons`, each changed by `edit`.
fn edited(automatons: &[&Automaton], edit: impl Fn(&mut Automaton)) -> Vec<Automaton> {
    automatons
        .iter()
        .map(|automaton| {
            let mut automaton = (*automaton).clone();
            edit(&mut automaton);
            automaton
        })
        .collect()
}

#[cfg(test)]
#[path = "check_test.rs"]
mod tests;
