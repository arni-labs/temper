//! The kernel's deletion tombstone: the event a delete writes, and what
//! applying it does to a record.

use super::types::{EntityEvent, EntityState};

impl EntityEvent {
    /// The kernel's deletion tombstone for a record last in `from_status`.
    pub(crate) fn tombstone(from_status: String) -> Self {
        Self {
            action: temper_runtime::persistence::TOMBSTONE_EVENT_TYPE.to_string(),
            from_status,
            to_status: "Deleted".to_string(),
            timestamp: temper_runtime::scheduler::sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
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
}
