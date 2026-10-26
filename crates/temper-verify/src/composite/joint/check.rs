//! The joint check an install and `temper verify` run.
//!
//! A rule over several records whose spec gives its proof the hints it
//! needs (lemmas beside an `[[invariant]]`, a measure beside a
//! `[[liveness]]` with `eventually`) is proved by induction
//! ([`crate::induct`]), for every run rather than the runs a bounded walk
//! reaches. The other rules, and any whose proof does not hold, are walked as
//! before and keep the walk's verdict, with why the proof did not hold.
//!
//! The proofs get the budget's time between them, and each walk the budget
//! it had before. A rule proved by induction is not checked for vacuity:
//! the proof covers every state, and does not ask which ones a run reaches.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Instant;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::Expr;

use super::liveness::{JointLivenessResult, LivenessOutcome, verify_joint_liveness};
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
    /// Proved by induction, for every run.
    Proved(RuleProof),
    /// Walked, with the proof that was tried and did not hold.
    Invariant(JointInvariantResult, Option<RuleProof>),
    /// Walked, with the proof that was tried and did not hold.
    Liveness(JointLivenessResult, Option<RuleProof>),
}

impl JointVerdict {
    /// Whether the rule holds in every run, as proved or walked in full.
    pub fn holds(&self) -> bool {
        match self {
            JointVerdict::Proved(_) => true,
            JointVerdict::Invariant(walked, _) => walked.holds(),
            JointVerdict::Liveness(walked, _) => walked.holds(),
        }
    }

    /// The rule, as `Type.name`.
    pub fn rule(&self) -> String {
        match self {
            JointVerdict::Proved(proof) => format!("{}.{}", proof.owner, proof.rule),
            JointVerdict::Invariant(walked, _) => format!("{}.{}", walked.entity, walked.invariant),
            JointVerdict::Liveness(walked, _) => format!("{}.{}", walked.entity, walked.name),
        }
    }

    /// The verdict in a word.
    pub fn tag(&self) -> &'static str {
        match self {
            JointVerdict::Proved(_) => "PROVED",
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
            JointVerdict::Proved(proof) => {
                return write!(
                    f,
                    "{} PROVED for every run by induction ({}; {} obligations in {} ms)",
                    self.rule(),
                    proof.scope.iter().cloned().collect::<Vec<_>>().join(" + "),
                    proof.obligations.len(),
                    proof.took.as_millis()
                );
            }
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

/// Check the rules over several records of `automatons`, as
/// [`verify_joint_invariants`] and [`verify_joint_liveness`] pick them,
/// with `policies` deciding each move as its sender: by induction where the
/// spec gives the hints, and by the walk for the rest.
pub fn check_joint(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    budget: Budget,
) -> JointCheck {
    let declared: BTreeSet<&str> = automatons
        .iter()
        .map(|a| a.automaton.name.as_str())
        .collect();
    // A rule that reads a type the app does not declare is reported by the
    // walk, which says so.
    let all_declared = |expr: &Expr| {
        expr.related_types()
            .iter()
            .all(|(entity_type, _)| declared.contains(entity_type.as_str()))
    };
    let mut hinted_invariants = BTreeSet::new();
    let mut hinted_liveness = BTreeSet::new();
    for automaton in automatons {
        let owner = &automaton.automaton.name;
        for invariant in &automaton.invariants {
            if !invariant.lemma.is_empty()
                && !invariant.assert.related_types().is_empty()
                && all_declared(&invariant.assert)
            {
                hinted_invariants.insert((owner.clone(), invariant.name.clone()));
            }
        }
        for liveness in &automaton.liveness {
            if let Some(goal) = &liveness.eventually
                && !liveness.measure.is_empty()
                && all_declared(goal)
            {
                hinted_liveness.insert((owner.clone(), liveness.name.clone()));
            }
        }
    }
    let proofs = if hinted_invariants.is_empty() && hinted_liveness.is_empty() {
        Vec::new()
    } else {
        prove_some(
            automatons,
            policies,
            &Hints::default(),
            |owner, rule| {
                let key = (owner.to_string(), rule.to_string());
                hinted_invariants.contains(&key) || hinted_liveness.contains(&key)
            },
            Some(Instant::now() + budget.time),
        )
    };
    let mut check = JointCheck::default();
    let mut proved = BTreeSet::new();
    let mut tried = BTreeMap::new();
    for proof in proofs {
        let key = (proof.owner.clone(), proof.rule.clone());
        if !proof.proved() {
            tried.insert(key, proof);
            continue;
        }
        if hinted_invariants.contains(&key) {
            check.invariants.push(JointVerdict::Proved(proof));
        } else {
            check.liveness.push(JointVerdict::Proved(proof));
        }
        proved.insert(key);
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
