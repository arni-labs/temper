//! Reading a record's large (overflowed) fields when a module asks for them.

use async_trait::async_trait;

/// Fetches the stored bytes of an overflowed record field for one WASM call.
///
/// `host_read_field` calls it the first time the module reads such a field and keeps the bytes for the rest of the
/// call, so a field the module never reads is never fetched and costs the call nothing. The reader charges each
/// fetch to the call's read budget and answers `None` when the field cannot be read within it or fails verification.
#[async_trait]
pub trait FieldBlobReader: Send + Sync {
    /// The bytes behind `reference`, the field's overflow descriptor as it stands in `entity_state.fields`.
    async fn read_field_blob(&self, reference: &serde_json::Value) -> Option<Vec<u8>>;
}
