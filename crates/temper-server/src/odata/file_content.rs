//! What a File (and a FileVersion) says of its bytes comes from bytes this server stored. Its `$value` PUT hashes the
//! bytes it keeps and writes `content_hash`, `size_bytes` and `has_content` itself; a read then fetches the bytes
//! stored under that hash. So no caller writes those fields: a create, PATCH or PUT naming one is refused, and a bound
//! action that carries a `content_hash` (`File.StreamUpdated`, `FileVersion.Create`) must name bytes this server
//! stored for the tenant.

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

/// A bound action whose `content_hash` names no bytes this server stored for the tenant: refused. Actions that carry
/// none are left alone.
pub(super) async fn refuse_unstored_hash(
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
        return None;
    }
    let hash = body.get("content_hash")?.as_str().unwrap_or_default();
    if hash.is_empty() {
        return None;
    }
    match state.file_bytes_stored(tenant, hash).await {
        Ok(true) => None,
        Ok(false) => Some(
            odata_error(
                StatusCode::CONFLICT,
                "FileBytesNotStored",
                &format!(
                    "{entity_type}.{action} names content_hash '{hash}', and this server stored no such bytes for this tenant; a File's bytes come from its $value PUT"
                ),
            )
            .into_response(),
        ),
        Err(error) => Some(
            odata_error(StatusCode::SERVICE_UNAVAILABLE, "FileStoreUnavailable", &error)
                .into_response(),
        ),
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
