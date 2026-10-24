//! The deletion check every query-projection write makes inside its write
//! transaction.

use crate::driver::{Transaction, params};
use temper_runtime::persistence::{
    PersistenceError, TOMBSTONE_EVENT_TYPE, is_tombstone_event, storage_error,
};

/// Whether the entity's journal leaves it without a projection row: it holds
/// the kernel's tombstone, or its newest event moves it to the `Deleted`
/// status, whose row the projection drops whichever action got it there.
/// Every projection write checks this inside its write transaction. The
/// journal commits the event before the projection row is removed, and SQLite
/// runs one write transaction at a time. So a write of older state either
/// lands before that event, and the removal then deletes it, or sees the event
/// and writes nothing. A deleted entity's row cannot come back.
///
/// Both reads are plain indexed lookups (`idx_events_deleted`,
/// `idx_events_entity`), and Rust decides the rule with the kernel's
/// [`is_tombstone_event`]. The embedded engine compiles each statement on the
/// caller's stack, and the rule written as SQL (`tombstone_sql`) took twice the
/// stack of a plain lookup in a debug build, on every projection write.
pub(super) async fn journal_leaves_deleted(
    tx: &Transaction<'_>,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<bool, PersistenceError> {
    // The literal matches `TOMBSTONE_EVENT_TYPE`; a bound value would not
    // match the partial index's predicate.
    let mut deleted_typed = tx
        .query(
            "SELECT payload FROM events \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
             AND event_type = 'Deleted'",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
    while let Some(row) = deleted_typed.next().await.map_err(storage_error)? {
        let payload = event_payload(&row.get::<String>(0).map_err(storage_error)?)?;
        if is_tombstone_event(TOMBSTONE_EVENT_TYPE, &payload) {
            return Ok(true);
        }
    }
    let mut newest = tx
        .query(
            "SELECT payload FROM events \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 \
             ORDER BY sequence_nr DESC LIMIT 1",
            params![tenant, entity_type, entity_id],
        )
        .await
        .map_err(storage_error)?;
    let Some(row) = newest.next().await.map_err(storage_error)? else {
        return Ok(false);
    };
    let payload = event_payload(&row.get::<String>(0).map_err(storage_error)?)?;
    Ok(payload.get("to_status").and_then(serde_json::Value::as_str) == Some(TOMBSTONE_EVENT_TYPE))
}

/// A journaled event's payload.
fn event_payload(payload_json: &str) -> Result<serde_json::Value, PersistenceError> {
    serde_json::from_str(payload_json).map_err(|e| PersistenceError::Serialization(e.to_string()))
}
