//! Telling a projection that changes nothing from one that does.
//!
//! Every action moves an entity's sequence and event counters, a poll that
//! changes nothing else included. A synced database's change log records each
//! row update with the whole row before and after it, so rewriting a large
//! catalog row for such an action costs the change log twice the row. A
//! projection whose status, fields and state (less those counters) hash the
//! same as the stored row's is not written at all.

use std::collections::BTreeMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// Keys of a projected state that count the entity's events rather than
/// describe it. Every action moves them, so they do not make a projection
/// differ; the row's `sequence_nr` column says which event it was projected
/// from.
const EVENT_COUNTER_KEYS: [&str; 5] = [
    "sequence_nr",
    "total_event_count",
    "events_since_snapshot",
    "last_snapshot_sequence_nr",
    "processed_idempotency_keys",
];

/// Hash of what a catalog row says about its entity: the status, the fields
/// as stored, and the projected state without its event counters. Two
/// projections with the same hash write the same row apart from those
/// counters and `sequence_nr`.
pub(super) fn projection_hash(
    status: &str,
    fields_json: &str,
    state: &serde_json::Value,
) -> String {
    let mut hasher = HashWriter(Sha256::new());
    hasher.0.update(status.as_bytes());
    hasher.0.update(b"\n");
    hasher.0.update(fields_json.as_bytes());
    hasher.0.update(b"\n");
    match state.as_object() {
        Some(entries) => {
            for (key, value) in entries {
                if EVENT_COUNTER_KEYS.contains(&key.as_str()) {
                    continue;
                }
                hasher.0.update(key.as_bytes());
                hasher.0.update(b"=");
                write_json(&mut hasher, value);
                hasher.0.update(b"\n");
            }
        }
        None => write_json(&mut hasher, state),
    }
    format!("{:x}", hasher.0.finalize())
}

fn write_json(hasher: &mut HashWriter, value: &serde_json::Value) {
    // Writing into a hasher cannot fail, and a `Value` always serializes.
    serde_json::to_writer(hasher, value).expect("a JSON value serializes into a hasher");
}

struct HashWriter(Sha256);

impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `(tenant, entity_type, entity_id)`.
pub(super) type ProjectionKey = (String, String, String);

/// For each entity whose newest projection matched its stored row and so was
/// not written, that projection's sequence.
///
/// A skipped write leaves the row's `sequence_nr` at its last change, so on
/// its own the row would accept an older projection with other content that
/// lands later (writes for one entity come from the queue, read repair and
/// backfill). This holds the newer sequence that refuses it. Writes still in
/// flight end with the process, so the record is kept in memory only.
///
/// One entry per entity whose last projection was skipped; a write of the
/// entity removes it.
#[derive(Debug, Default)]
pub(super) struct UnchangedProjections(Mutex<BTreeMap<ProjectionKey, u64>>);

impl UnchangedProjections {
    /// Whether a projection at `sequence_nr` is older than one already found
    /// unchanged, and so must not be written.
    pub(super) fn has_newer(&self, key: &ProjectionKey, sequence_nr: u64) -> bool {
        self.lock()
            .get(key)
            .is_some_and(|newest| *newest > sequence_nr)
    }

    /// Record that the projection at `sequence_nr` matched the stored row.
    pub(super) fn note(&self, key: ProjectionKey, sequence_nr: u64) {
        let mut newest = self.lock();
        let entry = newest.entry(key).or_insert(sequence_nr);
        *entry = (*entry).max(sequence_nr);
    }

    /// Forget an entry the row's own sequence now covers, after the row was
    /// written at `sequence_nr`.
    pub(super) fn written(&self, key: &ProjectionKey, sequence_nr: u64) {
        let mut newest = self.lock();
        if newest.get(key).is_some_and(|seq| *seq <= sequence_nr) {
            newest.remove(key);
        }
    }

    /// Forget an entity whose projection was removed.
    pub(super) fn removed(&self, key: &ProjectionKey) {
        self.lock().remove(key);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ProjectionKey, u64>> {
        // The map is only read and replaced under the lock; a panic while it
        // was held cannot leave a half-written entry.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
