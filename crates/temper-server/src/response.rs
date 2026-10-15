//! OData response formatting (JSON, XML, and binary stream).

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// An OData-formatted JSON response.
pub struct ODataResponse {
    /// HTTP status code for the response.
    pub status: StatusCode,
    /// JSON body payload.
    pub body: serde_json::Value,
}

impl IntoResponse for ODataResponse {
    fn into_response(self) -> Response {
        let mut response = axum::Json(self.body).into_response();
        *response.status_mut() = self.status;
        response
            .headers_mut()
            .insert("OData-Version", HeaderValue::from_static("4.0"));
        response.headers_mut().insert(
            "Content-Type",
            HeaderValue::from_static("application/json;odata.metadata=minimal"),
        );
        response
    }
}

/// An OData error response following the OData JSON error format.
pub fn odata_error(status: StatusCode, code: &str, message: &str) -> ODataResponse {
    ODataResponse {
        status,
        body: serde_json::json!({
            "error": {
                "code": code,
                "message": message
            }
        }),
    }
}

/// A resumable authorization denial with an authoritative decision identifier.
pub fn odata_denial(message: &str, decision_id: &str) -> ODataResponse {
    let mut response = odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", message);
    response.body["decision_id"] = serde_json::Value::String(decision_id.to_string());
    response
}

/// An OData XML response (for $metadata).
pub struct ODataXmlResponse {
    /// The XML body content.
    pub body: String,
}

impl IntoResponse for ODataXmlResponse {
    fn into_response(self) -> Response {
        let mut response = self.body.into_response();
        response
            .headers_mut()
            .insert("Content-Type", HeaderValue::from_static("application/xml"));
        response
            .headers_mut()
            .insert("OData-Version", HeaderValue::from_static("4.0"));
        response
    }
}

/// Cache header for hash-addressed `$value` bytes.
///
/// The object key is the content hash, so a matching ETag is the bytes
/// themselves for as long as the response stays private to this caller.
pub const IMMUTABLE_PRIVATE_CACHE: &str = "private, max-age=31536000, immutable";

/// Body of an OData `$value` response.
pub enum ODataStreamBody {
    /// Bytes already in memory (legacy and in-process callers).
    Bytes(Vec<u8>),
    /// Object-store body. `content_length` is the `Content-Length` header
    /// from the store, not a length computed by buffering the body first.
    Stream {
        /// Declared object size.
        content_length: u64,
        /// Chunked body.
        stream: crate::blob_store::BlobByteStream,
    },
}

impl ODataStreamBody {
    fn content_length(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::Stream { content_length, .. } => *content_length,
        }
    }

    fn into_http_body(self) -> axum::body::Body {
        match self {
            Self::Bytes(bytes) => axum::body::Body::from(bytes),
            Self::Stream { stream, .. } => axum::body::Body::from_stream(stream),
        }
    }
}

/// An OData stream response for `$value` endpoints.
///
/// Returns raw bytes with Content-Type, Content-Length, and an optional ETag.
/// A streamed body starts as soon as the store headers arrive.
pub struct ODataStreamResponse {
    /// HTTP status code for the response.
    pub status: StatusCode,
    /// Raw or streamed body payload.
    pub body: ODataStreamBody,
    /// MIME type for the Content-Type header.
    pub content_type: String,
    /// Optional ETag for cache validation (typically the content hash).
    pub etag: Option<String>,
    /// When set with an ETag, send [`IMMUTABLE_PRIVATE_CACHE`].
    pub cache_immutable: bool,
}

impl IntoResponse for ODataStreamResponse {
    fn into_response(self) -> Response {
        let content_length = self.body.content_length();
        let mut response = Response::builder()
            .status(self.status)
            .header("Content-Type", self.content_type)
            .header("Content-Length", content_length.to_string())
            .header("OData-Version", "4.0")
            .body(self.body.into_http_body())
            .expect("valid response"); // ci-ok: static headers always valid
        if let Some(etag) = self.etag {
            response.headers_mut().insert("ETag", quoted_etag(&etag));
            if self.cache_immutable {
                response.headers_mut().insert(
                    "Cache-Control",
                    HeaderValue::from_static(IMMUTABLE_PRIVATE_CACHE),
                );
            }
        }
        response
    }
}

/// `304 Not Modified` for a hash-addressed `$value` whose ETag still matches.
pub fn not_modified_stream(etag: &str) -> Response {
    Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header("OData-Version", "4.0")
        .header(
            "Cache-Control",
            HeaderValue::from_static(IMMUTABLE_PRIVATE_CACHE),
        )
        .header("ETag", quoted_etag(etag))
        .body(axum::body::Body::empty())
        .expect("valid 304") // ci-ok: static headers always valid
}

fn quoted_etag(etag: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{etag}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("\"unknown\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn denial_identity_survives_server_to_sandbox_round_trip() {
        let response = odata_denial("Order('PD-victim'). Decision PD-real created.", "PD-real");
        let encoded = serde_json::to_string(&response.body).unwrap();
        let parsed = temper_sandbox::helpers::format_authz_denied(&encoded).unwrap();
        assert_eq!(parsed["decision_id"], "PD-real");
    }

    #[test]
    fn odata_error_format() {
        let resp = odata_error(
            StatusCode::NOT_FOUND,
            "EntityNotFound",
            "Order 123 not found",
        );
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert_eq!(resp.body["error"]["code"], "EntityNotFound");
        assert_eq!(resp.body["error"]["message"], "Order 123 not found");
    }

    #[test]
    fn odata_json_response_headers() {
        let resp = ODataResponse {
            status: StatusCode::OK,
            body: serde_json::json!({"value": []}),
        };
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("OData-Version").unwrap(), "4.0");
        assert!(
            response
                .headers()
                .get("Content-Type")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("application/json")
        );
    }

    #[test]
    fn odata_xml_response_headers() {
        let resp = ODataXmlResponse {
            body: "<edmx:Edmx/>".to_string(),
        };
        let response = resp.into_response();
        assert_eq!(
            response.headers().get("Content-Type").unwrap(),
            "application/xml"
        );
        assert_eq!(response.headers().get("OData-Version").unwrap(), "4.0");
    }

    #[test]
    fn odata_error_custom_status() {
        let resp = odata_error(StatusCode::FORBIDDEN, "AuthzDenied", "not allowed");
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn odata_stream_response_headers() {
        let resp = ODataStreamResponse {
            status: StatusCode::OK,
            body: ODataStreamBody::Bytes(b"hello world".to_vec()),
            content_type: "application/octet-stream".to_string(),
            etag: Some("sha256:abc123".to_string()),
            cache_immutable: true,
        };
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("Content-Type").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(response.headers().get("Content-Length").unwrap(), "11");
        assert_eq!(response.headers().get("OData-Version").unwrap(), "4.0");
        assert!(
            response
                .headers()
                .get("ETag")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("sha256:abc123")
        );
        assert_eq!(
            response.headers().get("Cache-Control").unwrap(),
            "private, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn odata_stream_response_forwards_chunks_with_the_declared_length() {
        use futures_util::stream;
        let chunks = vec![
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"abc")),
            Ok(bytes::Bytes::from_static(b"def")),
        ];
        let resp = ODataStreamResponse {
            status: StatusCode::OK,
            body: ODataStreamBody::Stream {
                content_length: 6,
                stream: Box::pin(stream::iter(chunks)),
            },
            content_type: "text/plain".to_string(),
            etag: Some("sha256:abc".to_string()),
            cache_immutable: true,
        };
        let response = resp.into_response();
        assert_eq!(response.headers().get("Content-Length").unwrap(), "6");
        let collected = axum::body::to_bytes(response.into_body(), 64)
            .await
            .expect("collect streamed body");
        assert_eq!(collected.as_ref(), b"abcdef");
    }

    #[test]
    fn odata_stream_response_no_etag() {
        let resp = ODataStreamResponse {
            status: StatusCode::NO_CONTENT,
            body: ODataStreamBody::Bytes(vec![]),
            content_type: "application/toml".to_string(),
            etag: None,
            cache_immutable: false,
        };
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().get("ETag").is_none());
    }
}
