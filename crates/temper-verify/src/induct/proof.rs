//! What a proof found: each obligation's outcome, and the rule's verdict.

use std::collections::BTreeSet;
use std::time::Duration;

use super::Lemma;

/// One obligation's outcome.
#[derive(Debug, Clone)]
pub struct Obligation {
    /// The move, or `start`.
    pub name: String,
    pub took: Duration,
    pub outcome: Outcome,
    /// Whether the move can happen at all from a state where the clauses
    /// hold. One that cannot holds trivially, which a reader should see.
    pub possible: bool,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Holds,
    /// Some clauses fail after it: their names, and the counterexample.
    Fails {
        clauses: Vec<String>,
        cti: String,
    },
    Unknown,
}

/// A rule's proof.
#[derive(Debug, Clone)]
pub struct RuleProof {
    /// The type that declares the rule.
    pub owner: String,
    pub rule: String,
    pub scope: BTreeSet<String>,
    pub obligations: Vec<Obligation>,
    /// What the encoding could not express; each only weakens the proof.
    pub limits: Vec<String>,
    /// Effects that make moves the proof does not, such as a scheduled
    /// action; a proof with any is not a proof.
    pub unmodelled: Vec<String>,
    /// What the rule itself reads that the proof does not hold, each read
    /// as an unknown.
    pub unheld: BTreeSet<String>,
    /// The prover's own guesses the proof kept, when the agent gave no
    /// lemmas, written as lemmas the agent can pin beside the rule.
    pub found: Vec<Lemma>,
    /// Whether the solver shows that no state the proof covers meets the
    /// rule's premise (`premise => ...`), so the rule says nothing.
    pub premise_excluded: bool,
    /// What the solver refused, such as a lemma naming an unknown kind.
    pub errors: Vec<String>,
    /// Wall time, from reading the specs to the last check.
    pub took: Duration,
}

impl RuleProof {
    pub fn proved(&self) -> bool {
        self.shortfall().is_none()
    }

    /// Why the rule is not proved, in a line; `None` when it is.
    pub fn shortfall(&self) -> Option<String> {
        if let Some(error) = self.errors.first() {
            return Some(format!("the solver refused it: {error}"));
        }
        if let Some(effect) = self.unmodelled.first() {
            return Some(format!("{effect} makes a move the proof does not"));
        }
        self.obligations.iter().find_map(|o| match &o.outcome {
            Outcome::Holds => None,
            Outcome::Fails { clauses, .. } if clauses.is_empty() => {
                Some(format!("its clauses fail after {}", o.name))
            }
            Outcome::Fails { clauses, .. } => {
                Some(format!("{} fails after {}", clauses.join(", "), o.name))
            }
            Outcome::Unknown => Some(format!("the solver could not decide {}", o.name)),
        })
    }
}
