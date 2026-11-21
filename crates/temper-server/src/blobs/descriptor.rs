//! Validation for content-addressed field-overflow descriptors.

use serde_json::Value;

use super::{
    FIELD_OVERFLOW_BLOB_PREFIX, FIELD_OVERFLOW_ENCODING_KEY, FIELD_OVERFLOW_REF_KEY,
    FIELD_OVERFLOW_SIZE_KEY,
};

/// Validated reference to a field-overflow JSON object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldOverflowDescriptor<'a> {
    /// Content-addressed object-store key.
    pub key: &'a str,
    /// Lowercase SHA-256 of the serialized JSON object.
    pub sha256: &'a str,
    /// Serialized JSON byte length stored under `key`.
    pub serialized_bytes: u64,
}

/// Parse and validate a field-overflow descriptor.
pub fn field_overflow_descriptor(value: &Value) -> Option<FieldOverflowDescriptor<'_>> {
    let object = value.as_object()?;
    let key = object.get(FIELD_OVERFLOW_REF_KEY)?.as_str()?;
    let sha256 = field_overflow_sha256(key)?;
    if object.get(FIELD_OVERFLOW_ENCODING_KEY)?.as_str()? != "json" {
        return None;
    }
    let serialized_bytes = object.get(FIELD_OVERFLOW_SIZE_KEY)?.as_u64()?;
    if serialized_bytes == 0 {
        return None;
    }
    Some(FieldOverflowDescriptor {
        key,
        sha256,
        serialized_bytes,
    })
}

/// Extract the lowercase SHA-256 from a canonical field-overflow key.
pub fn field_overflow_sha256(key: &str) -> Option<&str> {
    let digest = key
        .strip_prefix(FIELD_OVERFLOW_BLOB_PREFIX)?
        .strip_suffix(".json")?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(digest)
}

/// Return whether `key` is a canonical field-overflow SHA-256 object key.
pub fn is_valid_field_overflow_key(key: &str) -> bool {
    field_overflow_sha256(key).is_some()
}

/// Whether `value` holds a field-overflow reference anywhere in it.
fn holds_field_reference(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(FIELD_OVERFLOW_REF_KEY) || map.values().any(holds_field_reference)
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
    sent.as_object()?.iter().find_map(|(name, value)| {
        (holds_field_reference(value) && held.get(name) != Some(value)).then(|| {
            format!(
                "'{name}' holds a stored-field reference ({FIELD_OVERFLOW_REF_KEY}): a write sends the value, and the kernel stores it"
            )
        })
    })
}

#[cfg(test)]
mod sent_reference_tests {
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
