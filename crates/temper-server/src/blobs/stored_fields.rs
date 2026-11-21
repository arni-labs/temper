//! Required stored values for record validation, independent of response budgets.

use std::collections::BTreeSet;

use serde_json::Value;

use super::field_overflow_descriptor;
use super::hydration::blob_bytes_match_key;
use super::read_source::{BlobReadSource, read_blob_ref_bytes};
use crate::blob_store::BlobReadBounded;

/// Why stored values could not be read back.
#[derive(Debug)]
pub(crate) enum StoredReadError {
    /// The blob store did not answer: another try may read them.
    Unavailable(String),
    /// A blob is missing, too long, fails its content address or is not
    /// JSON: no try will read it.
    Invalid(String),
}

impl From<StoredReadError> for String {
    fn from(error: StoredReadError) -> Self {
        match error {
            StoredReadError::Unavailable(error) | StoredReadError::Invalid(error) => error,
        }
    }
}

/// Resolve every referenced value in the selected fields, verifying its declared
/// length, content address and JSON. Each read is bounded by the stored length;
/// response/WASM byte and read budgets do not limit which records may be checked.
/// `None` means no selected field held a reference. Unreadable data fails closed.
pub(crate) async fn resolve_stored_fields(
    source: &BlobReadSource<'_>,
    fields: &Value,
    names: &BTreeSet<String>,
) -> Result<Option<Value>, String> {
    Ok(read_stored_fields(source, fields, names).await?)
}

async fn read_stored_fields(
    source: &BlobReadSource<'_>,
    fields: &Value,
    names: &BTreeSet<String>,
) -> Result<Option<Value>, StoredReadError> {
    let Some(map) = fields.as_object() else {
        return Ok(None);
    };
    let mut pointers = Vec::new();
    for name in names {
        if let Some(value) = map.get(name) {
            let escaped = name.replace('~', "~0").replace('/', "~1");
            collect_references(value, format!("/{escaped}"), &mut pointers);
        }
    }
    if pointers.is_empty() {
        return Ok(None);
    }
    let invalid = |error: &str| StoredReadError::Invalid(error.to_string());
    let mut resolved = fields.clone();
    for (pointer, key, declared_size) in pointers {
        let size = usize::try_from(declared_size)
            .map_err(|_| invalid("Stored field length does not fit in memory"))?;
        let bytes = match read_blob_ref_bytes(source, &key, size)
            .await
            .map_err(StoredReadError::Unavailable)?
        {
            BlobReadBounded::Found(bytes)
                if bytes.len() == size && blob_bytes_match_key(&key, &bytes) =>
            {
                bytes
            }
            _ => {
                return Err(invalid(
                    "Stored field blob could not be resolved and verified",
                ));
            }
        };
        let value = serde_json::from_slice(&bytes).map_err(|error| invalid(&error.to_string()))?;
        // The paths come from this unchanged copy of the stored record.
        *resolved
            .pointer_mut(&pointer)
            .expect("stored field path exists") = value;
    }
    Ok(Some(resolved))
}

/// `copied` with every reference the kernel stored in it read back and
/// verified, for the kernel's copy of one record's stored fields into a move
/// on another: a spawn's copied fields, a reaction's args. The target holds
/// none of the source's references, and a write may send only one its record
/// holds (`temper_jit::field_reference`), so the copy sends the value, as a
/// caller would, and the target stores it as its own. A copy of an inline
/// field and of one stored out of line answer alike. Read it in the try that
/// sends the move, so a store that does not answer
/// ([`StoredReadError::Unavailable`]) leaves the move owed for another try.
pub(super) async fn resolve_copied_values(
    source: &BlobReadSource<'_>,
    copied: &Value,
) -> Result<Option<Value>, StoredReadError> {
    let names: BTreeSet<String> = copied
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    read_stored_fields(source, copied, &names).await
}

impl crate::state::ServerState {
    /// [`resolve_copied_values`] from `tenant`'s blob store: `copied` as
    /// values, or as it is when it holds no reference.
    pub(crate) async fn copied_values(
        &self,
        tenant: &temper_runtime::tenant::TenantId,
        copied: &Value,
    ) -> Result<Value, StoredReadError> {
        let source = BlobReadSource::Tenant {
            state: self,
            tenant,
        };
        Ok(resolve_copied_values(&source, copied)
            .await?
            .unwrap_or_else(|| copied.clone()))
    }
}

fn collect_references(value: &Value, pointer: String, out: &mut Vec<(String, String, u64)>) {
    if let Some(descriptor) = field_overflow_descriptor(value) {
        out.push((
            pointer,
            descriptor.key.to_string(),
            descriptor.serialized_bytes,
        ));
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_references(value, format!("{pointer}/{index}"), out);
            }
        }
        Value::Object(map) => {
            for (name, value) in map {
                let escaped = name.replace('~', "~0").replace('/', "~1");
                collect_references(value, format!("{pointer}/{escaped}"), out);
            }
        }
        _ => {}
    }
}
