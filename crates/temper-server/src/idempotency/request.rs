//! What a request asked for, so an `Idempotency-Key` only ever answers a
//! retry of the request it was first used for.

use sha2::{Digest, Sha256};

/// The action a request named and a digest of its parameters.
///
/// Object keys are hashed in sorted order, so a retry that sends the same body
/// with its keys in another order is the same request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestFingerprint {
    action: String,
    params_digest: [u8; 32],
}

impl RequestFingerprint {
    /// Fingerprint `action` called with `params`.
    pub fn new(action: &str, params: &serde_json::Value) -> Self {
        let mut hasher = Sha256::new();
        hash_params(&mut hasher, params);
        Self {
            action: action.to_string(),
            params_digest: hasher.finalize().into(),
        }
    }

    /// The action the request named.
    pub fn action(&self) -> &str {
        &self.action
    }

    /// A short hex name for this request, for keys derived from it.
    pub fn short_hex(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.action.as_bytes());
        hasher.update([0]);
        hasher.update(self.params_digest);
        hasher.finalize()[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

enum Item<'a> {
    Value(&'a serde_json::Value),
    Key(&'a str),
}

/// Hash a JSON value with a tag and length before every part, so two
/// different values never feed the hasher the same bytes. Walked without
/// recursion.
fn hash_params(hasher: &mut Sha256, params: &serde_json::Value) {
    let text = |hasher: &mut Sha256, tag: u8, bytes: &[u8]| {
        hasher.update([tag]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    let mut pending = vec![Item::Value(params)];
    while let Some(item) = pending.pop() {
        match item {
            Item::Key(name) => text(hasher, b'k', name.as_bytes()),
            Item::Value(serde_json::Value::Null) => hasher.update(b"z"),
            Item::Value(serde_json::Value::Bool(value)) => {
                hasher.update(if *value { b"t" } else { b"f" })
            }
            Item::Value(serde_json::Value::Number(number)) => {
                text(hasher, b'n', number.to_string().as_bytes())
            }
            Item::Value(serde_json::Value::String(value)) => text(hasher, b's', value.as_bytes()),
            Item::Value(serde_json::Value::Array(items)) => {
                hasher.update(b"a");
                hasher.update((items.len() as u64).to_le_bytes());
                pending.extend(items.iter().rev().map(Item::Value));
            }
            Item::Value(serde_json::Value::Object(map)) => {
                hasher.update(b"o");
                hasher.update((map.len() as u64).to_le_bytes());
                let mut entries: Vec<_> = map.iter().collect();
                entries.sort_by(|(left, _), (right, _)| left.cmp(right));
                for (name, value) in entries.into_iter().rev() {
                    pending.push(Item::Value(value));
                    pending.push(Item::Key(name));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RequestFingerprint;
    use serde_json::json;

    #[test]
    fn the_same_body_in_another_key_order_is_the_same_request() {
        let first = RequestFingerprint::new("Note", &json!({"a": 1, "b": {"c": [1, "x"]}}));
        let second = RequestFingerprint::new("Note", &json!({"b": {"c": [1, "x"]}, "a": 1}));
        assert_eq!(first, second);
        assert_eq!(first.short_hex(), second.short_hex());
    }

    #[test]
    fn another_action_or_body_is_another_request() {
        let note = RequestFingerprint::new("Note", &json!({"a": 1}));
        assert_ne!(note, RequestFingerprint::new("Close", &json!({"a": 1})));
        assert_ne!(note, RequestFingerprint::new("Note", &json!({"a": 2})));
        assert_ne!(note, RequestFingerprint::new("Note", &json!({"a": "1"})));
        assert_ne!(
            RequestFingerprint::new("Note", &json!(["ab", "c"])),
            RequestFingerprint::new("Note", &json!(["a", "bc"]))
        );
        assert_eq!(note.short_hex().len(), 16);
    }
}
