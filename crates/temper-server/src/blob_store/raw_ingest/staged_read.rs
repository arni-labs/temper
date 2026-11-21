//! A raw upload's stored representation, read from its staging file before
//! anything is published, so the record it makes is checked first.

use futures_util::StreamExt as _;

use super::StagedBlob;
use crate::blob_store::BlobReadBounded;

impl StagedBlob {
    /// The bytes `put_staged_base64_json(_, self, prefix, _)` stores, read
    /// whole when they fit in `max_bytes` (as a bounded object-store read is).
    pub(crate) async fn base64_json_bounded(
        &self,
        prefix: &[u8],
        max_bytes: usize,
    ) -> Result<BlobReadBounded, String> {
        let len = self.base64_json_len(prefix)?;
        if len > max_bytes {
            return Ok(BlobReadBounded::TooLarge {
                actual_bytes: Some(len as u64),
            });
        }
        let mut bytes = Vec::with_capacity(len);
        let mut stream = self.base64_json_stream(prefix.to_vec());
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| format!("failed to read staged Blob body: {error}"))?;
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != len {
            return Err(format!(
                "staged Blob body read {} bytes; expected {len}",
                bytes.len()
            ));
        }
        Ok(BlobReadBounded::Found(bytes))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::stream;
    use temper_runtime::tenant::TenantId;

    use crate::blob_store::{BlobIngestBudget, BlobReadBounded, BlobStore};

    #[tokio::test]
    async fn the_staged_read_is_what_the_put_stores() {
        let dir = tempfile::tempdir().expect("dir");
        let store = BlobStore::local_fs(dir.path());
        let budget = BlobIngestBudget::files();
        let body = b"hello raw blob".to_vec();
        let mut permit = budget
            .try_reserve(&TenantId::default(), body.len())
            .expect("admitted");
        let prefix = format!("blob {}\0", body.len());
        let staged = store
            .stage_canonical_stream(
                Box::pin(stream::iter([Ok::<_, std::io::Error>(Bytes::from(
                    body.clone(),
                ))])),
                body.len(),
                prefix.as_bytes(),
                budget.progress_policy(),
                &mut permit,
            )
            .await
            .expect("staged");
        let descriptor = staged
            .base64_json_descriptor(prefix.as_bytes())
            .await
            .expect("descriptor");
        let BlobReadBounded::Found(read) = staged
            .base64_json_bounded(prefix.as_bytes(), descriptor.serialized_len)
            .await
            .expect("read")
        else {
            panic!("fits");
        };
        store
            .put_staged_base64_json(
                "k.json",
                &staged,
                prefix.as_bytes(),
                descriptor.serialized_len,
            )
            .await
            .expect("stored");
        assert_eq!(Some(read), store.get("k.json").await.expect("get"));
        assert!(matches!(
            staged
                .base64_json_bounded(prefix.as_bytes(), descriptor.serialized_len - 1)
                .await
                .expect("read"),
            BlobReadBounded::TooLarge { .. }
        ));
    }
}
