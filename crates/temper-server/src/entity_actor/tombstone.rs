//! The kernel's deletion tombstone: the event a delete writes, and what
//! applying it does to a record.

use super::types::{EntityEvent, EntityState};
use temper_runtime::persistence::PersistenceEnvelope;

/// The status the kernel's delete leaves a record in. A spec may name a state
/// of its own the same, so the status alone never says a record was deleted.
const TOMBSTONE_STATUS: &str = "Deleted";

/// Where to read a record's journal from after its snapshot at `snapshot_seq`
/// loaded: only what came after it, or the snapshot's own event too when the
/// snapshot may sit on a tombstone it does not name.
///
/// A snapshot written before records carried `last_event_tombstone` holds no
/// trace of the delete: passivating an actor loaded on a deleted id wrote one
/// at the tombstone, and a record loaded from it would be a record in a state
/// a spec names `Deleted`, readable and open to that spec's moves. Such a
/// snapshot cannot be told from one of a record a spec moved to `Deleted`, so
/// both read one more event.
pub(super) fn read_from_after_snapshot(state: &EntityState, snapshot_seq: u64) -> u64 {
    if state.status == TOMBSTONE_STATUS && !state.is_tombstoned() {
        snapshot_seq.saturating_sub(1)
    } else {
        snapshot_seq
    }
}

/// Take the snapshot's own event off the front of what
/// [`read_from_after_snapshot`] made the replay read, and mark the record
/// deleted if that event is the tombstone. What is left is the tail after the
/// snapshot.
pub(super) fn mark_deleted_by_snapshot_event(
    state: &mut EntityState,
    envelopes: &mut Vec<PersistenceEnvelope>,
    snapshot_seq: u64,
) {
    if envelopes
        .first()
        .is_none_or(|own| own.sequence_nr != snapshot_seq)
    {
        return;
    }
    let own = envelopes.remove(0);
    // The snapshot already holds what the event did to the record.
    state.last_event.last_event_tombstone = own.is_tombstone();
}

impl EntityEvent {
    /// The kernel's deletion tombstone for a record last in `from_status`.
    pub(crate) fn tombstone(from_status: String) -> Self {
        Self {
            action: temper_runtime::persistence::TOMBSTONE_EVENT_TYPE.to_string(),
            from_status,
            to_status: TOMBSTONE_STATUS.to_string(),
            timestamp: temper_runtime::scheduler::sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
            outcome: None,
            origin: crate::entity_actor::EventOrigin::internal(),
            kind: temper_runtime::persistence::EventKind::Tombstone,
        }
    }
}

impl EntityState {
    /// Apply the kernel's deletion tombstone: the record is `Deleted`.
    pub(crate) fn record_tombstone(&mut self, tombstone: EntityEvent) {
        debug_assert_eq!(
            tombstone.kind,
            temper_runtime::persistence::EventKind::Tombstone
        );
        self.status = tombstone.to_status.clone();
        if let Some(obj) = self.fields.as_object_mut() {
            obj.insert(
                "Status".to_string(),
                serde_json::Value::String(self.status.clone()),
            );
        }
        self.push_event_bounded(tombstone);
    }

    /// Whether the kernel's delete removed this record: its last event is the
    /// kernel's tombstone, which is terminal. Only that removes a record's
    /// catalog row and its id from the entity index. A spec action into a
    /// state named `Deleted` is an ordinary transition, and its record stays
    /// listed and readable like any other.
    ///
    /// The record says so itself (`last_event_tombstone`), so the answer is the
    /// same for a record loaded from a snapshot, which holds no events.
    pub(crate) fn is_tombstoned(&self) -> bool {
        self.last_event.last_event_tombstone
    }
}
