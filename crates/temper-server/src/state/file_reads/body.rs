//! Buffered or streamed body of an indexed file read.

use std::fmt;

use futures_util::StreamExt as _;

use crate::blob_store::BlobByteStream;

/// Bytes or an open object-store stream for an indexed file read.
pub enum IndexedFileBody {
    /// The object was fully buffered. In-process callers and the local
    /// equality tests use this.
    Bytes(Vec<u8>),
    /// The object is still arriving. `content_length` is the store's
    /// `Content-Length`, and `stream` yields the body without buffering it.
    Stream {
        /// Declared object size.
        content_length: u64,
        /// Chunked body.
        stream: BlobByteStream,
    },
}

impl fmt::Debug for IndexedFileBody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => formatter.debug_tuple("Bytes").field(bytes).finish(),
            Self::Stream { content_length, .. } => formatter
                .debug_struct("Stream")
                .field("content_length", content_length)
                .finish_non_exhaustive(),
        }
    }
}

impl PartialEq for IndexedFileBody {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bytes(left), Self::Bytes(right)) => left == right,
            _ => false,
        }
    }
}

/// Projection fields needed to answer a `$value` request before opening the blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexedFileDescriptor {
    /// Content hash, also the ETag.
    pub(crate) content_hash: String,
    /// Stored MIME type. Empty means `application/octet-stream`.
    pub(crate) mime_type: String,
    /// Declared size, when the projection recorded one.
    pub(crate) size_bytes: Option<u64>,
    /// Whether the entity claims to have content.
    pub(crate) has_content: bool,
}

/// Collect a buffered or streamed indexed body into bytes.
pub(crate) async fn materialize_indexed_body(body: IndexedFileBody) -> Result<Vec<u8>, String> {
    match body {
        IndexedFileBody::Bytes(bytes) => Ok(bytes),
        IndexedFileBody::Stream {
            content_length,
            mut stream,
        } => {
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| format!("file stream read failed: {error}"))?;
                bytes.extend_from_slice(&chunk);
            }
            if bytes.len() as u64 != content_length {
                return Err(format!(
                    "file stream ended at {} bytes; expected {content_length}",
                    bytes.len()
                ));
            }
            Ok(bytes)
        }
    }
}
