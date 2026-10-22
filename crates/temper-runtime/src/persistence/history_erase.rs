//! Erasing a deleted entity's history: what a store is asked to do and what it removed.

/// Remove everything a deleted entity's journal held before its tombstone.
///
/// The tombstone event stays at `tombstone_sequence_nr`, its payload replaced by
/// `tombstone_payload`, and `snapshot` (taken at the sequence just before it)
/// becomes the only snapshot, so replay starts there and needs no earlier event.
#[derive(Debug, Clone)]
pub struct HistoryErase<'a> {
    pub tenant: &'a str,
    pub entity_type: &'a str,
    pub entity_id: &'a str,
    /// The sequence of the `Deleted` event; the store refuses if the journal ends elsewhere.
    pub tombstone_sequence_nr: u64,
    /// The tombstone's payload without its action parameters (JSON).
    pub tombstone_payload: &'a str,
    /// The state at `tombstone_sequence_nr - 1`, holding no field but the id (JSON).
    pub snapshot: &'a [u8],
}

/// What one erase removed, by table.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct HistoryErased {
    pub events: u64,
    pub snapshots: u64,
    pub trajectories: u64,
    pub wasm_invocations: u64,
    pub pending_decisions: u64,
    /// Overflow blob keys the erased payloads referenced and no remaining event
    /// or snapshot of the same type still references.
    pub orphaned_blobs: Vec<String>,
}

/// Overflow blob keys (`field-overflow/sha256/<hex>.json`) named in `text`.
pub fn overflow_blob_keys(text: &str) -> std::collections::BTreeSet<String> {
    const PREFIX: &str = "field-overflow/sha256/";
    let mut keys = std::collections::BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let tail = &rest[at + PREFIX.len()..];
        let hex: String = tail.chars().take_while(char::is_ascii_hexdigit).collect();
        if hex.len() == 64 && tail[hex.len()..].starts_with(".json") {
            keys.insert(format!("{PREFIX}{hex}.json"));
        }
        rest = &tail[hex.len()..];
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::overflow_blob_keys;

    #[test]
    fn finds_each_overflow_key_once() {
        let hex = "a".repeat(64);
        let text = format!(
            r#"{{"x":{{"__temper_blob_ref":"field-overflow/sha256/{hex}.json"}},"y":"field-overflow/sha256/{hex}.json","z":"field-overflow/sha256/abc.json"}}"#
        );
        let keys = overflow_blob_keys(&text);
        assert_eq!(keys.len(), 1);
        assert!(keys.contains(&format!("field-overflow/sha256/{hex}.json")));
    }
}
