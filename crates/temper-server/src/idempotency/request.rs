//! Idempotency keys of the follow-up requests a keyed request starts.

use sha2::{Digest, Sha256};

/// The idempotency key of a follow-up request (a reaction, callback or
/// scheduled action) that the request under `parent` starts.
///
/// `label` names the follow-up (`callback:<action>`, `scheduled:<action>`)
/// and `detail` tells siblings with the same label apart. A retry of the parent
/// derives the same key. The key is a hash, not the parent with more text
/// appended, so it stays the same length however long a chain of follow-ups
/// runs: a self-rescheduling action keeps a key of constant size every cycle.
pub fn derived_idempotency_key(parent: &str, label: &str, detail: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [parent, label, detail] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    let digest: String = hasher.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("derived:{label}:{digest}")
}

#[cfg(test)]
mod tests {
    use super::derived_idempotency_key;

    #[test]
    fn a_derived_key_keeps_its_length_down_a_chain_and_repeats_for_a_retry() {
        let first = derived_idempotency_key("caller-key", "scheduled:Tick", "0");
        assert_eq!(
            first,
            derived_idempotency_key("caller-key", "scheduled:Tick", "0")
        );
        assert_ne!(
            first,
            derived_idempotency_key("caller-key", "scheduled:Tick", "1")
        );
        assert_ne!(
            first,
            derived_idempotency_key("other-key", "scheduled:Tick", "0")
        );

        let mut key = first.clone();
        for _ in 0..1_000 {
            let next = derived_idempotency_key(&key, "scheduled:Tick", "0");
            assert_ne!(next, key);
            assert_eq!(next.len(), first.len());
            key = next;
        }
    }
}
