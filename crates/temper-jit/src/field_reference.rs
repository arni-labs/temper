//! Stored-field references: made by the kernel, never written by a caller.
//!
//! A value too large to keep in its record is stored as a blob and the field
//! holds a reference to it (`{"__temper_blob_ref": key, ...}`). The check
//! lives with the transition table, so every move (on the server's actors,
//! the Postgres actor runtime, a spawn's initial action) refuses a forged one
//! through the same validation. A create is checked where a caller's fields
//! enter it, the OData collection create: the kernel's own creates (a raw
//! ingest, an install) store references they have just made.

use serde_json::Value;

/// The key that marks a JSON object as a stored-field reference.
pub const FIELD_REFERENCE_KEY: &str = "__temper_blob_ref";

/// Whether `value` holds a field-overflow reference anywhere in it.
fn holds_field_reference(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(FIELD_REFERENCE_KEY) || map.values().any(holds_field_reference)
        }
        Value::Array(items) => items.iter().any(holds_field_reference),
        _ => false,
    }
}

/// The first field of `sent` that holds a field-overflow reference its
/// record does not already hold there (`held` is the record's stored
/// fields), with the refusal to answer it with.
///
/// The kernel makes a reference when it stores a large value out of line,
/// and everything that reads the field (a read, a module's
/// `host_read_field`, a guard) reads the blob it names. One a caller wrote
/// would read any blob of the tenant whose key it named, whatever records
/// the caller may read. So a write sends values: it may send back the
/// reference its record holds for that field (a client echoing a record it
/// read), never a new one. Any object with the reference key counts, valid
/// or not, so no shape a later reader accepts can get past this.
pub fn sent_field_reference(sent: &Value, held: &Value) -> Option<String> {
    sent_field_reference_in(sent, held).map(|(_, reason)| reason)
}

/// [`sent_field_reference`] with the name of the field that holds it.
pub fn sent_field_reference_in(sent: &Value, held: &Value) -> Option<(String, String)> {
    sent.as_object()?.iter().find_map(|(name, value)| {
        (holds_field_reference(value) && held.get(name) != Some(value)).then(|| {
            (
                name.clone(),
                format!(
                    "'{name}' holds a stored-field reference ({FIELD_REFERENCE_KEY}): a write sends the value, and the kernel stores it"
                ),
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::sent_field_reference;

    const KEY: &str = "field-overflow/sha256/0000000000000000000000000000000000000000000000000000000000000000.json";

    fn reference() -> serde_json::Value {
        json!({"__temper_blob_ref": KEY, "__temper_blob_size": 10, "__temper_blob_encoding": "json"})
    }

    #[test]
    fn a_new_reference_is_refused_at_any_depth() {
        let held = json!({});
        assert!(sent_field_reference(&json!({"body": reference()}), &held).is_some());
        assert!(sent_field_reference(&json!({"body": {"inner": [reference()]}}), &held).is_some());
        assert!(
            sent_field_reference(&json!({"body": {"__temper_blob_ref": "not-a-key"}}), &held)
                .is_some()
        );
    }

    #[test]
    fn the_reference_the_record_holds_may_be_sent_back() {
        let held = json!({"body": reference()});
        assert_eq!(
            sent_field_reference(&json!({"body": reference()}), &held),
            None
        );
        assert!(sent_field_reference(&json!({"other": reference()}), &held).is_some());
    }

    #[test]
    fn plain_values_pass() {
        let sent = json!({"title": "x", "doc": {"blob_ref": KEY}, "n": 3});
        assert_eq!(sent_field_reference(&sent, &json!({})), None);
    }
}
