//! S3/R2 GET transport: pooled client, one retry, and a streamed body.

use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use reqwest::{Method, StatusCode};
use tracing::Instrument as _;

use super::limits::BLOB_READ_IDLE_TIMEOUT;
use super::{BlobByteStream, S3BlobStore};
use crate::aws_sigv4;
use crate::blob_transport_observability::{
    BlobTransportError, BlobTransportFinish, blob_transport_span, finish_blob_transport,
};

/// How long a presigned download URL stays valid.
const PRESIGNED_GET_TTL_SECS: u64 = 120;

/// Idle pooled connections are dropped after this, so a dead R2 socket is not
/// reused for the next download.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// TCP keepalive period. Combined with [`BLOB_READ_IDLE_TIMEOUT`], a half-open
/// connection fails instead of sitting in the pool.
const TCP_KEEPALIVE: Duration = Duration::from_secs(30);

pub(super) fn blob_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .read_timeout(BLOB_READ_IDLE_TIMEOUT)
        .build()
        .expect("static blob HTTP client configuration must be valid") // ci-ok: static reqwest builder options
}

enum GetAttempt {
    Response(reqwest::Response),
    /// Connect failed before any response bytes. Safe to try once more.
    Connect(String),
    Fatal(BlobTransportError),
}

impl S3BlobStore {
    /// Sign and send a GET, retrying once on connect failure or HTTP 5xx.
    ///
    /// The retry happens before any response body bytes are forwarded. A 5xx
    /// is dropped without reading it; a failure after the body has started is
    /// not retried.
    pub(super) async fn send_signed_get(
        &self,
        key: &str,
        total_timeout: Option<Duration>,
    ) -> Result<reqwest::Response, BlobTransportError> {
        let mut connect_error = None;
        for attempt in 0..2 {
            match self.send_signed_get_once(key, total_timeout).await {
                GetAttempt::Response(response) => {
                    if response.status().is_server_error() && attempt == 0 {
                        drop(response);
                        continue;
                    }
                    return Ok(response);
                }
                GetAttempt::Connect(error) if attempt == 0 => {
                    connect_error = Some(error);
                }
                GetAttempt::Connect(error) => {
                    return Err(BlobTransportError::message(error));
                }
                GetAttempt::Fatal(error) => return Err(error),
            }
        }
        Err(BlobTransportError::message(connect_error.unwrap_or_else(
            || format!("blob GET failed for '{key}' after one retry"),
        )))
    }

    async fn send_signed_get_once(&self, key: &str, total_timeout: Option<Duration>) -> GetAttempt {
        let url = self.object_url(key);
        let mut request = self.client.get(&url);
        if let Some(timeout) = total_timeout {
            request = request.timeout(timeout);
        }
        let headers = match self.signed_headers(Method::GET, &url) {
            Ok(headers) => headers,
            Err(error) => return GetAttempt::Fatal(BlobTransportError::message(error)),
        };
        for (header_name, header_value) in &headers {
            request = request.header(header_name, header_value);
        }
        match request.send().await {
            Ok(response) => GetAttempt::Response(response),
            Err(error) if error.is_connect() => {
                GetAttempt::Connect(format!("blob GET request failed for '{key}': {error}"))
            }
            Err(error) => GetAttempt::Fatal(BlobTransportError::message(format!(
                "blob GET request failed for '{key}': {error}"
            ))),
        }
    }

    /// Buffered GET. Callers that need the bytes still wait for the object,
    /// but the read is chunked with a per-chunk idle timeout instead of
    /// `response.bytes()`.
    pub(super) async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let started_at = Instant::now(); // determinism-ok: production blob transport metric only
        let span = blob_transport_span("get", "s3", 0);
        let result = async {
            let response = self.send_signed_get(key, None).await?;
            let status = response.status();
            if status == StatusCode::NOT_FOUND {
                return Ok((status, None, Some(started_at.elapsed())));
            }
            if !status.is_success() {
                return Err(BlobTransportError::status(
                    format!("blob GET failed for '{key}' with HTTP {status}"),
                    status,
                ));
            }
            let (bytes, ttfb) = read_body(response, key, started_at).await?;
            Ok((status, Some(bytes), Some(ttfb)))
        }
        .instrument(span.clone())
        .await;

        match result {
            Ok((status, bytes, ttfb)) => {
                let response_bytes = bytes.as_ref().map_or(0, |bytes| bytes.len() as u64);
                let outcome = if bytes.is_some() { "ok" } else { "not_found" };
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "get",
                    backend: "s3",
                    outcome,
                    status: Some(status),
                    request_bytes: 0,
                    response_bytes,
                    ttfb,
                });
                Ok(bytes)
            }
            Err(error) => {
                finish_blob_transport(BlobTransportFinish {
                    started_at,
                    span: &span,
                    operation: "get",
                    backend: "s3",
                    outcome: "error",
                    status: error.status,
                    request_bytes: 0,
                    response_bytes: 0,
                    ttfb: None,
                });
                Err(error.message)
            }
        }
    }

    /// Query-authenticated GET for this object, when the store has credentials.
    pub(super) fn presign_get(&self, key: &str) -> Option<String> {
        let access_key = self.access_key.as_deref()?;
        let secret_key = self.secret_key.as_deref()?;
        let amz_date = aws_sigv4::amz_date_now();
        aws_sigv4::presign_get_url(&aws_sigv4::PresignedGetRequest {
            url: &self.object_url(key),
            region: "auto",
            service: "s3",
            access_key,
            secret_key,
            amz_date: &amz_date,
            expires_secs: PRESIGNED_GET_TTL_SECS,
        })
        .ok()
    }
}

async fn read_body(
    response: reqwest::Response,
    key: &str,
    started_at: Instant,
) -> Result<(Vec<u8>, Duration), BlobTransportError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    let mut ttfb = None;
    loop {
        let next = tokio::time::timeout(BLOB_READ_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| {
                BlobTransportError::message(format!("blob GET idle deadline exceeded for '{key}'"))
            })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            BlobTransportError::message(format!("blob GET body read failed for '{key}': {error}"))
        })?;
        if ttfb.is_none() && !chunk.is_empty() {
            ttfb = Some(started_at.elapsed());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, ttfb.unwrap_or_else(|| started_at.elapsed())))
}

/// Records time-to-first-byte and the bytes actually pulled when a streamed
/// GET is consumed or dropped.
pub(super) struct GetStreamObservation {
    started_at: Instant,
    span: tracing::Span,
    status: StatusCode,
    bytes_read: u64,
    ttfb: Option<Duration>,
    finished: bool,
}

impl GetStreamObservation {
    pub(super) fn new(started_at: Instant, span: tracing::Span, status: StatusCode) -> Self {
        Self {
            started_at,
            span,
            status,
            bytes_read: 0,
            ttfb: None,
            finished: false,
        }
    }

    pub(super) fn note_chunk(&mut self, len: usize) {
        if self.ttfb.is_none() && len > 0 {
            self.ttfb = Some(self.started_at.elapsed());
        }
        self.bytes_read = self.bytes_read.saturating_add(len as u64);
    }

    pub(super) fn succeed(&mut self) {
        self.finish("ok");
    }

    pub(super) fn fail(&mut self) {
        self.finish("error");
    }

    fn finish(&mut self, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        let ttfb = self.ttfb.or_else(|| Some(self.started_at.elapsed()));
        finish_blob_transport(BlobTransportFinish {
            started_at: self.started_at,
            span: &self.span,
            operation: "get",
            backend: "s3",
            outcome,
            status: Some(self.status),
            request_bytes: 0,
            response_bytes: self.bytes_read,
            ttfb,
        });
    }
}

impl Drop for GetStreamObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.finish("incomplete");
        }
    }
}

/// Wrap `source` so the transport metric records TTFB and `response_bytes`
/// as the caller pulls chunks. `source` is named so the signature stays readable.
pub(super) fn observe_get_stream(
    mut source: BlobByteStream,
    observation: GetStreamObservation,
) -> BlobByteStream {
    Box::pin(async_stream::try_stream! {
        let mut observation = observation;
        loop {
            match source.next().await {
                Some(Ok(chunk)) => {
                    observation.note_chunk(chunk.len());
                    yield chunk;
                }
                Some(Err(error)) => {
                    observation.fail();
                    Err(error)?;
                }
                None => break,
            }
        }
        observation.succeed();
    })
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt as _;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::BlobStore;

    async fn s3_store(server: &MockServer) -> BlobStore {
        BlobStore::s3(
            server.uri(),
            "bucket",
            None,
            None,
            std::env::temp_dir(),
            None,
        )
    }

    #[tokio::test]
    async fn streamed_get_bytes_match_the_object() {
        let server = MockServer::start().await;
        let body = b"streamed-object-bytes-0123456789";
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
            .expect(1)
            .mount(&server)
            .await;

        let store = s3_store(&server).await;
        let bytes = store
            .get("object.bin")
            .await
            .expect("get")
            .expect("object exists");
        assert_eq!(bytes, body);
    }

    #[tokio::test]
    async fn get_retries_once_on_5xx_before_reading_a_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"recovered"))
            .expect(1)
            .mount(&server)
            .await;

        let store = s3_store(&server).await;
        let bytes = store.get("object.bin").await.expect("get").expect("body");
        assert_eq!(bytes, b"recovered");
        server.verify().await;
    }

    #[tokio::test]
    async fn open_download_stream_matches_the_object_bytes() {
        let server = MockServer::start().await;
        let body = b"chunk-me-please-this-is-the-object";
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.as_slice())
                    .insert_header("content-length", body.len().to_string()),
            )
            .mount(&server)
            .await;

        let store = s3_store(&server).await;
        let opened = store
            .get_stream("object.bin", 1024 * 1024)
            .await
            .expect("open stream");
        let super::super::BlobStreamRead::Found(stream) = opened else {
            panic!("expected a found stream");
        };
        assert_eq!(stream.content_length(), body.len() as u64);
        let collected = stream
            .into_stream()
            .try_collect::<Vec<_>>()
            .await
            .expect("collect stream")
            .concat();
        assert_eq!(collected, body);
    }
}
