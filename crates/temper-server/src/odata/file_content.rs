//! What a File (and a FileVersion) says of its bytes comes from bytes this server stored. Its `$value` PUT hashes the
//! bytes it keeps and writes `content_hash`, `size_bytes` and `has_content` itself; a read then fetches the bytes
//! stored under that hash. So no caller writes those fields: a create, PATCH or PUT naming one is refused, a bound
//! action that carries a `content_hash` (`File.StreamUpdated`, `FileVersion.Create`) must name bytes this server
//! stored for the tenant, with their size, and any other bound action names none of them.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_runtime::tenant::TenantId;

use crate::response::odata_error;
use crate::state::ServerState;

/// The fields only a File's stored bytes decide.
const CONTENT_FIELDS: [&str; 3] = ["content_hash", "has_content", "size_bytes"];

fn holds_file_content(entity_type: &str) -> bool {
    matches!(entity_type, "File" | "FileVersion")
}

/// A create, PATCH or PUT body that names a File's content fields: refused.
pub(super) fn refuse_content_fields(
    entity_type: &str,
    body: &serde_json::Value,
) -> Option<Response> {
    if !holds_file_content(entity_type) {
        return None;
    }
    let named: Vec<&str> = CONTENT_FIELDS
        .into_iter()
        .filter(|field| body.get(field).is_some())
        .collect();
    (!named.is_empty()).then(|| {
        odata_error(
            StatusCode::BAD_REQUEST,
            "FileContentFields",
            &format!(
                "{} of a {entity_type} come only from the bytes its $value PUT stores; send the bytes there instead",
                named.join(", ")
            ),
        )
        .into_response()
    })
}

/// A bound action on a File or FileVersion that says what its bytes are: refused unless they are bytes this server
/// stored. `File.StreamUpdated` and `FileVersion.Create` may name a `content_hash` of stored bytes, with their
/// `size_bytes` when the store knows it; `StreamUpdated` must name one. Any other action names none of the fields.
pub(super) async fn refuse_caller_content(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    action: &str,
    body: &serde_json::Value,
) -> Option<Response> {
    if !holds_file_content(entity_type) {
        return None;
    }
    let action = action.rsplit('.').next().unwrap_or(action);
    if !matches!(
        (entity_type, action),
        ("File", "StreamUpdated") | ("FileVersion", "Create")
    ) {
        return refuse_content_fields(entity_type, body);
    }
    let not_stored = |why: String| {
        Some(
            odata_error(
                StatusCode::CONFLICT,
                "FileBytesNotStored",
                &format!("{why}; a File's bytes come from its $value PUT"),
            )
            .into_response(),
        )
    };
    if body.get("has_content").is_some() {
        return not_stored(format!("{entity_type}.{action} does not take has_content"));
    }
    let hash = body
        .get("content_hash")
        .and_then(|hash| hash.as_str())
        .unwrap_or_default();
    if hash.is_empty() {
        if entity_type == "File" {
            return not_stored(
                "File.StreamUpdated needs the content_hash of bytes this server stored".to_string(),
            );
        }
        return refuse_content_fields(entity_type, body);
    }
    let stored = match state.stored_file_bytes(tenant, hash).await {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            return not_stored(format!(
                "{entity_type}.{action} names content_hash '{hash}', and this server stored no such bytes for this tenant"
            ));
        }
        Err(error) => {
            return Some(
                odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "FileStoreUnavailable",
                    &error,
                )
                .into_response(),
            );
        }
    };
    let said = body
        .get("size_bytes")
        .and_then(|size| size.as_u64().or_else(|| size.as_str()?.parse().ok()));
    match stored.size {
        Some(size) if said != Some(size) => not_stored(format!(
            "{entity_type}.{action} must say size_bytes {size}, the size of the stored bytes"
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_content_fields_are_never_a_callers() {
        for entity_type in ["File", "FileVersion"] {
            for body in [
                serde_json::json!({ "Id": "f1", "content_hash": "sha256:00" }),
                serde_json::json!({ "has_content": true }),
                serde_json::json!({ "size_bytes": 42, "mime_type": "image/png" }),
            ] {
                let refused = refuse_content_fields(entity_type, &body).expect("refused");
                assert_eq!(
                    refused.status(),
                    StatusCode::BAD_REQUEST,
                    "{entity_type} {body}"
                );
            }
            assert!(
                refuse_content_fields(
                    entity_type,
                    &serde_json::json!({ "Id": "f1", "mime_type": "image/png" })
                )
                .is_none()
            );
        }
        // Any other type keeps its own fields of those names.
        assert!(
            refuse_content_fields("Picture", &serde_json::json!({ "content_hash": "x" })).is_none()
        );
    }
}
