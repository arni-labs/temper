//! Streaming `$value` open. The blob I/O slot is released once headers arrive.

use temper_runtime::tenant::TenantId;

use super::super::ServerState;
use super::super::file_read_projection::{
    file_projection_from_state, file_version_projection_from_state,
};
use super::{
    BlobBodyLoad, FileProjectionMeta, IndexedFileDescriptor, IndexedFileStreamRead, StreamKind,
};

impl ServerState {
    /// Open a File `$value` as a stream. The blob I/O slot is released once
    /// the object headers arrive; the caller forwards `Content::Stream`.
    pub(crate) async fn open_file_stream_indexed(
        &self,
        tenant: &TenantId,
        file_id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        self.open_stream_indexed(tenant, StreamKind::File, file_id)
            .await
    }

    /// Open a FileVersion `$value` as a stream.
    pub(crate) async fn open_file_version_stream_indexed(
        &self,
        tenant: &TenantId,
        file_version_id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        self.open_stream_indexed(tenant, StreamKind::Version, file_version_id)
            .await
    }

    /// Hash, type, and size for a `$value` request, without reading the blob.
    pub(crate) async fn indexed_file_descriptor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        id: &str,
    ) -> Result<Option<IndexedFileDescriptor>, String> {
        let kind = match entity_type {
            "File" => StreamKind::File,
            "FileVersion" => StreamKind::Version,
            _ => return Ok(None),
        };
        Ok(self
            .load_stream_meta(tenant, &kind, id)
            .await?
            .map(|meta| IndexedFileDescriptor {
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
                size_bytes: meta.size_bytes,
                has_content: meta.has_content,
            }))
    }

    async fn open_stream_indexed(
        &self,
        tenant: &TenantId,
        kind: StreamKind,
        id: &str,
    ) -> Result<IndexedFileStreamRead, String> {
        let Some(meta) = self.load_stream_meta(tenant, &kind, id).await? else {
            return Ok(IndexedFileStreamRead::MissingIndex);
        };
        let read = self
            .read_indexed_stream_from_meta(tenant, meta, BlobBodyLoad::Stream)
            .await?;
        self.reconcile_indexed_stream_read(tenant, kind, id, read, BlobBodyLoad::Stream)
            .await
    }

    async fn load_stream_meta(
        &self,
        tenant: &TenantId,
        kind: &StreamKind,
        id: &str,
    ) -> Result<Option<FileProjectionMeta>, String> {
        match kind {
            StreamKind::File => {
                let indexed = self
                    .load_file_projection_metadata_from_index(tenant, &[id.to_string()])
                    .await?;
                if let Some(meta) = indexed.get(id).cloned() {
                    return Ok(Some(meta));
                }
                let Ok(resp) = self.get_tenant_entity_state(tenant, "File", id).await else {
                    return Ok(None);
                };
                Ok(Some(file_projection_from_state(&resp.state.fields)))
            }
            StreamKind::Version => {
                let indexed = self
                    .load_file_version_projection_metadata_from_index(tenant, &[id.to_string()])
                    .await?;
                if let Some(meta) = indexed.get(id).cloned() {
                    return Ok(Some(meta));
                }
                let Ok(resp) = self
                    .get_tenant_entity_state(tenant, "FileVersion", id)
                    .await
                else {
                    return Ok(None);
                };
                Ok(Some(file_version_projection_from_state(&resp.state.fields)))
            }
        }
    }
}
