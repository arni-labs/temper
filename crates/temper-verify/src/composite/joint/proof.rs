//! One rule's proof, the digest its verdict is kept by, and what its proof
//! by induction reads.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use sha2::{Digest, Sha256};
use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;

use super::liveness::liveness_proofs;
use super::{InvariantProof, LivenessProof, invariant_proofs};
use crate::induct::{Answered, RuleProof, prove_rule};

/// The proof of one rule: an invariant, or a liveness property with
/// `eventually`. It holds all its walk reads, so its walk's verdict depends
/// on nothing else.
#[derive(Debug)]
pub enum JointProof {
    Invariant(InvariantProof),
    Liveness(LivenessProof),
}

impl JointProof {
    /// The rule, as `Type.name`.
    pub fn name(&self) -> String {
        match self {
            Self::Invariant(proof) => format!("{}.{}", proof.entity, proof.invariant.name),
            Self::Liveness(proof) => format!("{}.{}", proof.entity, proof.name),
        }
    }

    /// The digest of all its walk reads: the rule, and its joint model cut
    /// to what it reads, with the senders and the Cedar that decides them.
    /// Two proofs with one digest walk the same model for the same rule, so
    /// a change to a spec that the cut leaves out leaves the digest as it
    /// was.
    pub fn digest(&self) -> String {
        // Every type a proof holds keeps its entries in order (no hash
        // maps), so this text is all of the value, the same each time. A
        // kernel that changes those types changes the text, and its proofs
        // walk once again.
        let text = format!("{self:?}");
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    /// The same invariant's proof with every sender allowed, when Cedar
    /// decides this one. None for an `eventually` rule: more senders may
    /// keep a record from its goal, so its verdict there says nothing of
    /// the app's Cedar.
    pub(super) fn open(&self) -> Option<JointProof> {
        let JointProof::Invariant(rule) = self else {
            return None;
        };
        let open = rule.app.open()?;
        invariant_proofs(&open, |owner, name| {
            owner == rule.entity && name == rule.invariant.name
        })
        .pop()
        .map(JointProof::Invariant)
    }
}

/// The proof of every invariant and `eventually` rule of `automatons`, with
/// `policies` deciding each move as its sender: the invariants', then the
/// liveness properties'.
pub fn joint_proofs(automatons: &[&Automaton], policies: Option<&SendAnalysis>) -> Vec<JointProof> {
    let app = App::new(automatons, policies);
    invariant_proofs(&app, |_, _| true)
        .into_iter()
        .map(JointProof::Invariant)
        .chain(liveness_proofs(&app).into_iter().map(JointProof::Liveness))
        .collect()
}

/// What a proof by induction reads besides its rule: every type of the app,
/// its Cedar, and what Cedar answered the app's other proofs. The proofs of
/// one app share it.
///
/// A digest prints it as nothing. A proof by induction covers every move the
/// walk makes and more, so a rule it proves holds in every state its walk
/// reaches: a verdict kept by the walk's model is that model's verdict,
/// whichever of the two reached it.
#[derive(Clone)]
pub(super) struct App(Arc<Parts>);

struct Parts {
    automatons: Vec<Automaton>,
    policies: Option<SendAnalysis>,
    answered: Answered,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("App")
    }
}

impl App {
    pub(super) fn new(automatons: &[&Automaton], policies: Option<&SendAnalysis>) -> Self {
        Self(Arc::new(Parts {
            automatons: automatons.iter().map(|a| (*a).clone()).collect(),
            policies: policies.cloned(),
            answered: Answered::default(),
        }))
    }

    /// The app's types by name.
    pub(super) fn by_name(&self) -> BTreeMap<&str, &Automaton> {
        self.0
            .automatons
            .iter()
            .map(|a| (a.automaton.name.as_str(), a))
            .collect()
    }

    /// The app with every sender allowed, when it has Cedar.
    fn open(&self) -> Option<Self> {
        self.0.policies.as_ref()?;
        Some(Self(Arc::new(Parts {
            automatons: self.0.automatons.clone(),
            policies: None,
            answered: Answered::default(),
        })))
    }

    pub(super) fn policies(&self) -> Option<&SendAnalysis> {
        self.0.policies.as_ref()
    }

    /// Prove `owner`'s rule `rule` by induction, stopping at `deadline`.
    pub(super) fn prove(
        &self,
        owner: &str,
        rule: &str,
        deadline: Option<Instant>,
    ) -> Option<RuleProof> {
        let refs: Vec<&Automaton> = self.0.automatons.iter().collect();
        prove_rule(
            &refs,
            self.policies(),
            &self.0.answered,
            (owner, rule),
            deadline,
        )
    }
}
