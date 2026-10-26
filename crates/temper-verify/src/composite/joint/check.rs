//! The joint check an install and `temper verify` run.
//!
//! Each invariant, and each `eventually` rule, is proved by induction first
//! ([`crate::induct`]), for every run rather than the runs a bounded walk
//! reaches: with the lemmas written beside it, or with none, the rule at
//! every state and then the prover's own guesses, which a verdict lists so
//! they can be written beside the rule. An `eventually` rule is tried when
//! it carries its measure, which nothing guesses. A rule whose proof does not
//! hold is walked as before and keeps the walk's verdict, with why the proof
//! did not hold.
//!
//! A rule whose premise no run meets says nothing, proved or not
//! ([`vacuity`](super::vacuity)): the solver rules that out first, and a
//! walk to the first state that meets the premise is the fallback. Each
//! rule is proved on its own ([`JointProof`]), so an install keeps each
//! verdict by what that rule reads.

use std::fmt;
use std::time::{Duration, Instant};

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;

use super::liveness::{JointLivenessResult, LivenessOutcome};
use super::proof::{JointProof, joint_proofs};
use super::room::Bound;
use super::vacuity::{Premise, premise_met, premise_reached};
use super::{JointInvariantResult, JointOutcome};
use crate::induct::RuleProof;

/// How long a rule's proof by induction runs before its walk decides it.
/// Over quantified lemmas the solver may search without end, and a proof
/// that has not finished says nothing of the rule.
pub const PROOF_TIME: Duration = Duration::from_secs(120);

/// What the joint check found, rule by rule.
#[derive(Debug, Clone, Default)]
pub struct JointCheck {
    /// Each `[[invariant]]`.
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
            JointVerdict::Proved(_, premise) => premise.met(),
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
            JointVerdict::Proved(_, Premise::None | Premise::Met { .. }) => "PROVED",
            JointVerdict::Proved(_, Premise::Excluded { .. } | Premise::Never { .. }) => "VACUOUS",
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
    let proved = format!(
        "{rule} PROVED for every run by induction ({scope}; {} obligations in {} ms",
        proof.obligations.len(),
        proof.took.as_millis()
    );
    match premise {
        Premise::None => write!(f, "{proved})")?,
        Premise::Met { states, took } => write!(
            f,
            "{proved}; a run meets its premise, found after {states} joint states in {} ms)",
            took.as_millis()
        )?,
        Premise::Excluded { premise } => write!(
            f,
            "{rule} PROVES NOTHING: proved by induction, and the proof shows no state meets its premise `{premise}` ({scope})"
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

impl JointProof {
    /// Prove the rule by induction, for every run, and walk it when the
    /// proof does not hold. A proof still running after `proof_time` is
    /// left to the walk: over quantified lemmas the solver may search
    /// without end. A rule that reads a type the app does not declare is
    /// left to the walk, which says so, and so is an `eventually` rule
    /// without a measure, which nothing guesses.
    pub fn check(&self, bound: Bound<'_>, proof_time: Duration) -> JointVerdict {
        let deadline = Instant::now().checked_add(proof_time);
        match self {
            JointProof::Invariant(rule) => {
                let tried = match &rule.model {
                    Ok(_) => rule.app.prove(&rule.entity, &rule.invariant.name, deadline),
                    Err(_) => None,
                };
                match tried {
                    Some(proof) if proof.proved() => {
                        let assert = &rule.invariant.assert;
                        let premise = premise_met(&rule.model, &rule.entity, assert, &proof, bound);
                        JointVerdict::Proved(proof, premise)
                    }
                    tried => JointVerdict::Invariant(rule.walk(bound), tried),
                }
            }
            JointProof::Liveness(rule) => {
                let tried = match &rule.model {
                    Ok(_) if rule.measured => rule.app.prove(&rule.entity, &rule.name, deadline),
                    _ => None,
                };
                match tried {
                    Some(proof) if proof.proved() => {
                        let premise =
                            premise_reached(&rule.model, &rule.entity, &rule.goal, &proof, bound);
                        JointVerdict::Proved(proof, premise)
                    }
                    tried => JointVerdict::Liveness(rule.walk(bound), tried),
                }
            }
        }
    }
}

/// Check every invariant and `eventually` rule of `automatons`
/// ([`joint_proofs`]),
/// with `policies` deciding each move as its sender: by induction first, and
/// by the walk where the proof does not hold ([`JointProof::check`]).
pub fn check_joint(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    bound: Bound<'_>,
    proof_time: Duration,
) -> JointCheck {
    let mut check = JointCheck::default();
    for proof in joint_proofs(automatons, policies) {
        let verdict = proof.check(bound, proof_time);
        match proof {
            JointProof::Invariant(_) => check.invariants.push(verdict),
            JointProof::Liveness(_) => check.liveness.push(verdict),
        }
    }
    check
}

#[cfg(test)]
#[path = "check_test.rs"]
mod tests;
