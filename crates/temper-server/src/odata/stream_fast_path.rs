use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_runtime::tenant::TenantId;

use crate::response::{ODataStreamBody, ODataStreamResponse, not_modified_stream, odata_error};
use crate::state::{IndexedFileBody, IndexedFileStreamRead, ServerState};

/// Files at least this large are redirected to a presigned object URL when
/// the store can sign one. Smaller files are streamed through Temper unless
/// the caller asks with `?redirect=1`.
const VALUE_REDIRECT_MIN_BYTES: u64 = 1024 * 1024;

/// Request conditions that apply only to `GET $value`.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileValueRequest {
    /// Raw `If-None-Match` header, when the client sent one.
    pub(crate) if_none_match: Option<String>,
    /// `?redirect=1` asks for a presigned URL even for a small object.
    pub(crate) redirect: bool,
}

pub(crate) async fn try_file_stream_fast_path(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
    request: &FileValueRequest,
) -> Option<Response> {
    if !matches!(entity_type, "File" | "FileVersion") {
        return None;
    }
    let descriptor = match state
        .indexed_file_descriptor(tenant, entity_type, key)
        .await
    {
        Ok(descriptor) => descriptor,
        Err(error) => return Some(index_error_response(tenant, entity_type, key, error)),
    };
    let Some(descriptor) = descriptor else {
        return missing_index(tenant, entity_type, key);
    };
    if !descriptor.has_content || descriptor.content_hash.is_empty() {
        return Some(no_content_response(set_name, key));
    }
    if if_none_match_satisfied(request.if_none_match.as_deref(), &descriptor.content_hash) {
        return Some(not_modified_stream(&descriptor.content_hash));
    }
    if should_redirect(descriptor.size_bytes, request.redirect)
        && let Ok(Some(location)) = state
            .presign_file_blob(tenant, &descriptor.content_hash)
            .await
    {
        return Some(redirect_response(&location));
    }

    let opened = match entity_type {
        "File" => state.open_file_stream_indexed(tenant, key).await,
        "FileVersion" => state.open_file_version_stream_indexed(tenant, key).await,
        _ => return None,
    };
    match opened {
        Ok(read) => indexed_read_response_or_fallback(tenant, set_name, entity_type, key, read),
        Err(error) => Some(index_error_response(tenant, entity_type, key, error)),
    }
}

fn should_redirect(size_bytes: Option<u64>, redirect: bool) -> bool {
    redirect || size_bytes.is_some_and(|size| size >= VALUE_REDIRECT_MIN_BYTES)
}

/// `If-None-Match` hits when any tag is `*`, the quoted content hash, or the
/// raw hash. Weak validators (`W/"..."`) are compared on the opaque tag.
pub(crate) fn if_none_match_satisfied(header: Option<&str>, content_hash: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    let quoted = format!("\"{content_hash}\"");
    header.split(',').any(|raw| {
        let tag = raw.trim();
        if tag == "*" {
            return true;
        }
        let tag = tag.strip_prefix("W/").unwrap_or(tag).trim();
        tag == quoted || tag == content_hash
    })
}

fn indexed_read_response_or_fallback(
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
    read: IndexedFileStreamRead,
) -> Option<Response> {
    match read {
        IndexedFileStreamRead::Content {
            content_hash,
            mime_type,
            body,
        } => Some(content_response(content_hash, mime_type, body)),
        IndexedFileStreamRead::NoContent { .. } => Some(no_content_response(set_name, key)),
        IndexedFileStreamRead::MissingIndex => missing_index(tenant, entity_type, key),
        IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
            tracing::warn!(
                tenant = %tenant,
                entity_type = %entity_type,
                entity_id = %key,
                content_hash = %content_hash,
                "file stream projection blob missing; falling back to actor/WASM materialization"
            );
            None
        }
    }
}

fn content_response(content_hash: String, mime_type: String, body: IndexedFileBody) -> Response {
    let body = match body {
        IndexedFileBody::Bytes(bytes) => ODataStreamBody::Bytes(bytes),
        IndexedFileBody::Stream {
            content_length,
            stream,
        } => ODataStreamBody::Stream {
            content_length,
            stream,
        },
    };
    ODataStreamResponse {
        status: StatusCode::OK,
        body,
        content_type: stream_content_type(&mime_type),
        etag: Some(content_hash),
        cache_immutable: true,
    }
    .into_response()
}

fn redirect_response(location: &str) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header("Location", location)
        .header("Cache-Control", "private, max-age=0")
        .header("OData-Version", "4.0")
        .body(axum::body::Body::empty())
        .expect("static redirect headers are valid") // ci-ok: static headers always valid
}

fn no_content_response(set_name: &str, key: &str) -> Response {
    odata_error(
        StatusCode::NOT_FOUND,
        "NoContent",
        &format!("{set_name}('{key}') has no content yet"),
    )
    .into_response()
}

fn missing_index(tenant: &TenantId, entity_type: &str, key: &str) -> Option<Response> {
    tracing::warn!(
        tenant = %tenant,
        entity_type = %entity_type,
        entity_id = %key,
        "file stream projection missing; falling back to actor/WASM materialization"
    );
    None
}

fn index_error_response(
    tenant: &TenantId,
    entity_type: &str,
    key: &str,
    error: String,
) -> Response {
    tracing::error!(
        tenant = %tenant,
        entity_type = %entity_type,
        entity_id = %key,
        %error,
        "file stream projection read failed"
    );
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "FileReadIndexUnavailable",
        &error,
    )
    .into_response()
}

fn stream_content_type(mime_type: &str) -> String {
    if mime_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime_type.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_none_match_accepts_the_quoted_content_hash() {
        assert!(if_none_match_satisfied(
            Some("\"sha256:abc\""),
            "sha256:abc"
        ));
        assert!(if_none_match_satisfied(
            Some("W/\"sha256:abc\", \"other\""),
            "sha256:abc"
        ));
        assert!(if_none_match_satisfied(Some("*"), "sha256:abc"));
        assert!(!if_none_match_satisfied(
            Some("\"sha256:other\""),
            "sha256:abc"
        ));
        assert!(!if_none_match_satisfied(None, "sha256:abc"));
    }

    #[test]
    fn redirect_threshold_is_about_one_megabyte() {
        assert!(!should_redirect(Some(1024), false));
        assert!(should_redirect(Some(VALUE_REDIRECT_MIN_BYTES), false));
        assert!(should_redirect(Some(1), true));
        assert!(!should_redirect(None, false));
    }

    #[test]
    fn immutable_cache_header_is_private_and_long_lived() {
        assert_eq!(
            crate::response::IMMUTABLE_PRIVATE_CACHE,
            "private, max-age=31536000, immutable"
        );
    }
}
