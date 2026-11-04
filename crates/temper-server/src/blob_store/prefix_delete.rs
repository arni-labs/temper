//! Removing every object of an object store's namespace: ListObjectsV2 pages
//! under the store's key prefix, then DeleteObjects batches of what each page
//! names.

use base64::Engine as _;
use md5::{Digest as _, Md5};
use reqwest::Method;
use serde::Deserialize;

use super::limits::BLOB_BUFFERED_OPERATION_TIMEOUT;
use super::{BlobStore, BlobStoreBackend, S3BlobStore};
use crate::aws_sigv4::query_encode;

/// The most keys one DeleteObjects call may name: the S3 API's limit, which
/// R2 keeps too.
const DELETE_OBJECTS_MAX_KEYS: usize = 1000;

impl BlobStore {
    /// The key prefix an object store keeps this store's objects under, such
    /// as a tenant's `tenants/<sha256 of its name>`; `None` for a local store.
    pub(crate) fn object_key_prefix(&self) -> Option<&str> {
        match &self.backend {
            BlobStoreBackend::LocalFs { .. } => None,
            BlobStoreBackend::S3(store) => store.key_prefix.as_deref(),
        }
    }

    /// Remove every object under this object store's key prefix, returning
    /// how many it removed. A store without a prefix is a whole bucket, which
    /// is never removed; a local store's files are removed as a directory.
    /// A list or delete the provider refuses fails the call with its answer,
    /// and objects not yet removed stay.
    pub(crate) async fn remove_every_object(&self) -> Result<usize, String> {
        match &self.backend {
            BlobStoreBackend::LocalFs { .. } => {
                Err("a local store's objects are removed as its directory".to_string())
            }
            BlobStoreBackend::S3(store) => store.remove_every_object().await,
        }
    }

    /// The first object under this object store's key prefix, if any.
    pub(crate) async fn first_object(&self) -> Result<Option<String>, String> {
        match &self.backend {
            BlobStoreBackend::LocalFs { .. } => {
                Err("a local store's objects are read as its directory".to_string())
            }
            BlobStoreBackend::S3(store) => {
                let prefix = store.namespace_prefix()?;
                let page = store.list_page(&prefix, None, Some(1)).await?;
                Ok(page.contents.into_iter().next().map(|object| object.key))
            }
        }
    }
}

/// One ListObjectsV2 page.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListBucketResult {
    #[serde(default)]
    contents: Vec<ListedObject>,
    #[serde(default)]
    is_truncated: bool,
    next_continuation_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedObject {
    key: String,
}

/// A DeleteObjects answer in quiet mode: only the keys it did not delete.
#[derive(Deserialize)]
struct DeleteResult {
    #[serde(default, rename = "Error")]
    errors: Vec<DeleteError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DeleteError {
    key: String,
    code: String,
    #[serde(default)]
    message: String,
}

impl S3BlobStore {
    /// The store's key prefix as a list prefix (`.../`). A store without one
    /// is a whole bucket, which is never removed.
    fn namespace_prefix(&self) -> Result<String, String> {
        self.key_prefix
            .as_deref()
            .map(|prefix| prefix.trim_matches('/'))
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| format!("{prefix}/"))
            .ok_or_else(|| {
                "an object store without a key prefix is a whole bucket, which is never removed"
                    .to_string()
            })
    }

    async fn remove_every_object(&self) -> Result<usize, String> {
        let prefix = self.namespace_prefix()?;
        let mut removed = 0;
        let mut continuation: Option<String> = None;
        loop {
            let page = self
                .list_page(&prefix, continuation.as_deref(), None)
                .await?;
            let keys: Vec<String> = page.contents.into_iter().map(|object| object.key).collect();
            if let Some(stray) = keys.iter().find(|key| !key.starts_with(&prefix)) {
                return Err(format!(
                    "the object store listed '{stray}' under prefix '{prefix}'; nothing more was removed"
                ));
            }
            for batch in keys.chunks(DELETE_OBJECTS_MAX_KEYS) {
                self.delete_batch(batch).await?;
                removed += batch.len();
            }
            match page.next_continuation_token.filter(|_| page.is_truncated) {
                Some(token) => continuation = Some(token),
                None => return Ok(removed),
            }
        }
    }

    async fn list_page(
        &self,
        prefix: &str,
        continuation: Option<&str>,
        max_keys: Option<usize>,
    ) -> Result<ListBucketResult, String> {
        let mut url = format!(
            "{}/{}?list-type=2&prefix={}",
            self.endpoint,
            self.bucket,
            query_encode(prefix)
        );
        if let Some(token) = continuation {
            url.push_str(&format!("&continuation-token={}", query_encode(token)));
        }
        if let Some(max_keys) = max_keys {
            url.push_str(&format!("&max-keys={max_keys}"));
        }
        let body = self.send(Method::GET, &url, None, "list").await?;
        quick_xml::de::from_str(&body).map_err(|error| {
            format!("object store list under '{prefix}' was not understood: {error}")
        })
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), String> {
        assert!(
            keys.len() <= DELETE_OBJECTS_MAX_KEYS,
            "a DeleteObjects batch names at most {DELETE_OBJECTS_MAX_KEYS} keys"
        );
        let mut body =
            String::from(r#"<?xml version="1.0" encoding="UTF-8"?><Delete><Quiet>true</Quiet>"#);
        for key in keys {
            body.push_str("<Object><Key>");
            body.push_str(&quick_xml::escape::escape(key.as_str()));
            body.push_str("</Key></Object>");
        }
        body.push_str("</Delete>");
        let url = format!("{}/{}?delete", self.endpoint, self.bucket);
        let answer = self.send(Method::POST, &url, Some(body), "delete").await?;
        let result: DeleteResult = quick_xml::de::from_str(&answer)
            .map_err(|error| format!("object store delete answer was not understood: {error}"))?;
        match result.errors.first() {
            None => Ok(()),
            Some(first) => Err(format!(
                "the object store did not delete {} of {} objects, such as '{}': {} {}",
                result.errors.len(),
                keys.len(),
                first.key,
                first.code,
                first.message
            )),
        }
    }

    /// Send one signed list or delete request and return its body; an answer
    /// other than success (such as a 403 for credentials without the
    /// permission) fails with the provider's status and body.
    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<String>,
        operation: &str,
    ) -> Result<String, String> {
        let mut request = self.client.request(method.clone(), url);
        if let Some(body) = body {
            let md5 =
                base64::engine::general_purpose::STANDARD.encode(Md5::digest(body.as_bytes()));
            request = request
                .header("content-md5", md5)
                .header(reqwest::header::CONTENT_TYPE, "application/xml")
                .body(body);
        }
        for (name, value) in &self.signed_headers(method, url)? {
            request = request.header(name, value);
        }
        let response = tokio::time::timeout(BLOB_BUFFERED_OPERATION_TIMEOUT, request.send())
            .await
            .map_err(|_| format!("object store {operation} timed out"))?
            .map_err(|error| format!("object store {operation} failed: {error}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| format!("object store {operation} answer unreadable: {error}"))?;
        if !status.is_success() {
            return Err(format!(
                "object store refused {operation} with HTTP {status}: {text}"
            ));
        }
        Ok(text)
    }
}
