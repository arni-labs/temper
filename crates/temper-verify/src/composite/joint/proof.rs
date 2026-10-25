//! One walk of the joint checks, and the digest its verdict is kept by.

use sha2::{Digest, Sha256};
use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;

use super::{InvariantProof, LivenessProof, invariant_proofs, liveness_proofs};

/// One walk of the joint checks: an entity's invariants that read related
/// records, or one liveness property. It holds all its walk reads, so its
/// verdict depends on nothing else.
#[derive(Debug)]
pub enum JointProof {
    Invariants(InvariantProof),
    Liveness(LivenessProof),
}

impl JointProof {
    /// What it proves, in words.
    pub fn name(&self) -> String {
        match self {
            Self::Invariants(proof) => format!("{}'s rules over several records", proof.entity),
            Self::Liveness(proof) => format!("{}.{}", proof.entity, proof.name),
        }
    }

    /// The digest of all its walk reads: the properties, and their joint
    /// model cut to what they read, with what Cedar lets each of its
    /// senders send. Two proofs with one digest walk the same model for the
    /// same properties, so a change to a spec or to Cedar that the cut
    /// leaves out leaves the digest as it was.
    pub fn digest(&self) -> String {
        // Every type a proof holds derives Debug and keeps its entries in
        // order (no hash maps), so this text is all of the value, the same
        // each time. A kernel that changes those types changes the text,
        // and its proofs walk once again.
        let text = format!("{self:?}");
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }
}

/// Every walk the joint checks make for `automatons`: the invariants'
/// ([`invariant_proofs`]), then the liveness properties'
/// ([`liveness_proofs`]).
pub fn joint_proofs(automatons: &[&Automaton], policies: Option<&SendAnalysis>) -> Vec<JointProof> {
    invariant_proofs(automatons, policies)
        .into_iter()
        .map(JointProof::Invariants)
        .chain(
            liveness_proofs(automatons, policies)
                .into_iter()
                .map(JointProof::Liveness),
        )
        .collect()
}
