use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use temper_runtime::tenant::TenantId;

use super::{
    BlobReadBounded, BlobStore, DEFAULT_BLOB_BUCKET, TENANT_NAMESPACES_DIR,
    is_local_internal_blob_endpoint,
};
use crate::state::ServerState;

impl ServerState {
    pub(crate) fn blob_store_for_tenant(&self, tenant: &TenantId) -> Result<BlobStore, String> {
        let store = self.unscoped_blob_store_for_tenant(tenant)?;
        // The default tenant's root and prefix also hold every other tenant's namespace (namespace.rs).
        Ok(match tenant_object_namespace(tenant) {
            Some(_) => store,
            None => store.holding_tenant_namespaces(),
        })
    }

    /// Why `key` names no object this tenant may use, or `None` when it may. A refused key is the caller's to fix,
    /// unlike a store that fails; a server without a configured store reports that on the read or write itself.
    pub(crate) fn refused_blob_key(&self, tenant: &TenantId, key: &str) -> Option<String> {
        self.blob_store_for_tenant(tenant)
            .ok()
            .and_then(|store| store.check_key(key).err())
    }

    fn unscoped_blob_store_for_tenant(&self, tenant: &TenantId) -> Result<BlobStore, String> {
        if let Some(vault) = self.secrets_vault.as_ref()
            && let Some(endpoint) = vault.get_secret(tenant.as_str(), "blob_endpoint")
            && !endpoint.trim().is_empty()
        {
            if is_local_internal_blob_endpoint(&endpoint) {
                return self.local_blob_store(tenant).ok_or_else(|| {
                    "internal DB-backed blob endpoint is disabled; set TEMPER_LOCAL_BLOB_DIR or configure BLOB_ENDPOINT for R2/S3"
                        .to_string()
                });
            }
            let bucket = vault
                .get_secret(tenant.as_str(), "blob_bucket")
                .unwrap_or_else(|| DEFAULT_BLOB_BUCKET.to_string());
            let prefix = vault.get_secret(tenant.as_str(), "blob_prefix");
            let link = match (
                vault.get_secret(tenant.as_str(), "blob_link_access_key"),
                vault.get_secret(tenant.as_str(), "blob_link_secret_key"),
            ) {
                (Some(access_key), Some(secret_key)) => Some(super::LinkCredentials {
                    access_key,
                    secret_key,
                    session_token: vault.get_secret(tenant.as_str(), "blob_link_session_token"),
                }),
                _ => None,
            };
            return Ok(BlobStore::s3(
                endpoint,
                bucket,
                vault.get_secret(tenant.as_str(), "blob_access_key"),
                vault.get_secret(tenant.as_str(), "blob_secret_key"),
                self.blob_staging_root(tenant),
                object_key_prefix(prefix.as_deref(), tenant),
            )
            .with_session_token(vault.get_secret(tenant.as_str(), "blob_session_token"))
            .with_link_credentials(link));
        }
        self.local_blob_store(tenant).ok_or_else(|| {
            "blob object store is not configured; set BLOB_ENDPOINT/BLOB_BUCKET/BLOB_ACCESS_KEY/BLOB_SECRET_KEY or TEMPER_LOCAL_BLOB_DIR"
                .to_string()
        })
    }

    pub(crate) async fn put_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        let store = self.blob_store_for_tenant(tenant)?;
        store.put_if_absent(key, body, ttl).await?;
        self.put_metadata_blob_shadow(tenant, key, body, ttl).await
    }

    /// Write a File's bytes to their content-addressed key. When the tenant's store is a remote object store (R2/S3),
    /// that store alone keeps them: they are never copied into the database, which for an app's own Temper is its one
    /// SQLite file. A local store keeps the database copy the default tenant has always had, unless this server keeps
    /// Files only in an object store (`files_need_object_store`): then nothing is kept and the write is refused.
    pub(crate) async fn put_file_content_blob(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
    ) -> Result<(), String> {
        if let Some(why) = self.files_refused_here(tenant) {
            return Err(format!("{why}; nothing was kept"));
        }
        let store = self.blob_store_for_tenant(tenant)?;
        store.put_content_addressed(key, body, None).await?;
        if store.is_object_store() {
            return Ok(());
        }
        self.put_metadata_blob_shadow(tenant, key, body, None).await
    }

    /// Why this server neither keeps nor serves a tenant's File bytes in `store`: it keeps Files only in an object
    /// store (`files_need_object_store`) and `store` is a local one. A local store may be shared by every server on
    /// one machine (`$HOME/.local/share/temper/blobs` for each default tenant), so its bytes are not this server's.
    pub(crate) fn files_refused_here(&self, tenant: &TenantId) -> Option<String> {
        if !self.files_need_object_store {
            return None;
        }
        let remote = self
            .blob_store_for_tenant(tenant)
            .is_ok_and(|store| store.is_object_store());
        (!remote).then(|| {
            format!(
                "this server keeps Files only in an object store, and tenant '{tenant}' has none (no blob_endpoint in its vault)"
            )
        })
    }

    /// A blob from the database copy the default tenant keeps beside a local store, and nowhere else: the copy is
    /// this server's own (an app's Temper has its own database), so it serves Files kept before
    /// `files_need_object_store` without the shared local store.
    pub(crate) async fn get_own_database_blob(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        if tenant != &TenantId::default() {
            return Ok(None);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(None);
        };
        store
            .get_blob(key)
            .await
            .map_err(|error| format!("legacy DB blob read failed for '{key}': {error}"))
    }

    pub async fn get_blob_with_legacy_fallback(
        &self,
        tenant: &TenantId,
        key: &str,
    ) -> Result<Option<Vec<u8>>, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get(key).await {
                Ok(Some(bytes)) => return Ok(Some(bytes)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%key, %error, "object blob store read failed; trying legacy DB blob fallback");
                }
            },
            Err(error) => {
                tracing::debug!(%key, %error, "object blob store unavailable; trying legacy DB blob fallback");
            }
        }
        if tenant != &TenantId::default() {
            return Ok(None);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(None);
        };
        store
            .get_blob(key)
            .await
            .map_err(|error| format!("legacy DB blob read failed for '{key}': {error}"))
    }

    pub(crate) async fn get_blob_with_legacy_fallback_bounded(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: usize,
    ) -> Result<BlobReadBounded, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get_bounded(key, max_bytes).await {
                Ok(BlobReadBounded::Found(bytes)) => {
                    return Ok(BlobReadBounded::Found(bytes));
                }
                Ok(BlobReadBounded::TooLarge { actual_bytes }) => {
                    return Ok(BlobReadBounded::TooLarge { actual_bytes });
                }
                Ok(BlobReadBounded::Missing) => {}
                Err(error) => {
                    tracing::warn!(%key, %error, "bounded object blob read failed; trying legacy DB fallback");
                }
            },
            Err(error) => {
                tracing::debug!(%key, %error, "object blob store unavailable; trying bounded legacy DB fallback");
            }
        }
        if tenant != &TenantId::default() {
            return Ok(BlobReadBounded::Missing);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(BlobReadBounded::Missing);
        };
        store
            .get_blob_if_size_at_most(key, max_bytes)
            .await
            .map(|bytes| match bytes {
                Some(bytes) => BlobReadBounded::Found(bytes),
                None => BlobReadBounded::Missing,
            })
            .map_err(|error| format!("bounded legacy DB blob read failed for '{key}': {error}"))
    }

    /// Open a tenant-scoped object-store blob as a bounded stream.
    ///
    /// Objects within the caller's ceiling fall back to the legacy database
    /// store when the object store does not have them, so this agrees with
    /// [`Self::get_blob_with_legacy_fallback`] about what exists. Anything above
    /// the ceiling is still refused there rather than buffered, which is why
    /// the fallback asks the store to bound the read instead of reading first.
    pub async fn stream_blob_object(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: u64,
    ) -> Result<super::BlobStreamRead, String> {
        match self.blob_store_for_tenant(tenant) {
            Ok(store) => match store.get_stream(key, max_bytes).await {
                Ok(super::BlobStreamRead::Missing) => {}
                other => return other,
            },
            Err(error) => {
                tracing::debug!(
                    %key,
                    %error,
                    "object blob store unavailable for streaming read; trying legacy DB blob fallback"
                );
            }
        }

        // Objects written before the object-store migration live in the legacy
        // DB blob store. `get_blob_with_legacy_fallback` already consults it, so
        // without the same fallback here the streaming and non-streaming reads
        // disagree about which objects exist: a Genesis app bundle reported
        // "field overflow blob ... not found" for a blob the HTTP blob route
        // served successfully at the same moment.
        self.legacy_blob_stream(tenant, key, max_bytes).await
    }

    /// Read one object from the legacy DB blob store as a bounded stream.
    async fn legacy_blob_stream(
        &self,
        tenant: &TenantId,
        key: &str,
        max_bytes: u64,
    ) -> Result<super::BlobStreamRead, String> {
        if tenant != &TenantId::default() {
            return Ok(super::BlobStreamRead::Missing);
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(super::BlobStreamRead::Missing);
        };
        // Bound before materializing. The legacy interface is buffered, which is
        // why streaming reads skipped it originally; asking the store to refuse
        // anything over the caller's ceiling keeps that intent — the fallback
        // never pulls a large object into memory, it just stops pretending
        // small ones do not exist.
        let ceiling = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        // Hold the same blob I/O permit the object-store path takes. The read is
        // bounded per caller but still buffered, so without the permit a burst
        // of legacy reads is unbounded in aggregate — the ceiling limits one
        // read, the semaphore limits how many happen at once.
        let _permit = tokio::time::timeout(
            super::BLOB_IO_QUEUE_TIMEOUT,
            super::blob_io_semaphore().acquire_owned(),
        )
        .await
        .map_err(|_| format!("legacy DB blob read for '{key}' timed out queueing for blob I/O"))?
        .map_err(|error| format!("legacy DB blob I/O semaphore closed for '{key}': {error}"))?;
        // Bounded in time as well as size, like every other buffered blob
        // operation. This is an external database read holding a blob I/O
        // permit: without a deadline a hung store would keep the permit and
        // starve the readers queued behind it.
        let Some(bytes) = tokio::time::timeout(
            super::BLOB_BUFFERED_OPERATION_TIMEOUT,
            store.get_blob_if_size_at_most(key, ceiling),
        )
        .await
        .map_err(|_| format!("legacy DB blob read for '{key}' timed out"))?
        .map_err(|error| format!("legacy DB blob read failed for '{key}': {error}"))?
        else {
            return Ok(super::BlobStreamRead::Missing);
        };
        tracing::debug!(%key, bytes = bytes.len(), "served blob from the legacy DB store");
        Ok(super::BlobStreamRead::Found(
            super::BlobObjectStream::from_bytes(bytes),
        ))
    }

    fn local_blob_store(&self, tenant: &TenantId) -> Option<BlobStore> {
        if let Ok(root) = std::env::var("TEMPER_LOCAL_BLOB_DIR") // determinism-ok: deployment config read
            && !root.trim().is_empty()
        {
            return Some(BlobStore::local_fs(tenant_blob_root(root.into(), tenant)));
        }
        if !self.data_dir.as_os_str().is_empty() {
            return Some(BlobStore::local_fs(tenant_blob_root(
                self.data_dir.join("blobs"),
                tenant,
            )));
        }
        None
    }

    fn blob_staging_root(&self, tenant: &TenantId) -> PathBuf {
        let root = if !self.data_dir.as_os_str().is_empty() {
            self.data_dir.join("blob-ingest-staging")
        } else {
            std::env::temp_dir() // determinism-ok: production object-store I/O staging path
                .join("temper-blob-ingest-staging")
        };
        tenant_blob_root(root, tenant)
    }

    async fn put_metadata_blob_shadow(
        &self,
        tenant: &TenantId,
        key: &str,
        body: &[u8],
        ttl: Option<Duration>,
    ) -> Result<(), String> {
        if tenant != &TenantId::default() {
            return Ok(());
        }
        let Some(store) = self.metadata_store_for_tenant(tenant.as_str()).await else {
            return Ok(());
        };
        store
            .put_blob_with_ttl(key, body, ttl)
            .await
            .map_err(|error| format!("metadata blob shadow write failed for '{key}': {error}"))
    }
}

/// Where a tenant's objects sit in the bucket: under the deployment's own prefix (`blob_prefix`, such as one app's
/// folder in a bucket many servers share), then, for any tenant but `default`, under `tenants/<sha256 of its name>`.
/// The default tenant keeps the bare prefix, where its older objects are, and its store refuses keys under `tenants/`.
fn object_key_prefix(prefix: Option<&str>, tenant: &TenantId) -> Option<String> {
    let prefix = prefix
        .map(|p| p.trim_matches('/'))
        .filter(|p| !p.is_empty());
    let namespace = tenant_object_namespace(tenant)
        .map(|namespace| format!("{TENANT_NAMESPACES_DIR}/{namespace}"));
    match (prefix, namespace) {
        (Some(prefix), Some(namespace)) => Some(format!("{prefix}/{namespace}")),
        (Some(prefix), None) => Some(prefix.to_string()),
        (None, namespace) => namespace,
    }
}

fn tenant_object_namespace(tenant: &TenantId) -> Option<String> {
    (tenant != &TenantId::default())
        .then(|| super::hex_lower(&Sha256::digest(tenant.as_str().as_bytes())))
}

fn tenant_blob_root(root: PathBuf, tenant: &TenantId) -> PathBuf {
    match tenant_object_namespace(tenant) {
        Some(namespace) => root.join(TENANT_NAMESPACES_DIR).join(namespace),
        None => root,
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::CsdlDocument;

    use super::*;

    #[test]
    fn tenant_object_namespaces_are_stable_and_default_compatible() {
        assert_eq!(tenant_object_namespace(&TenantId::default()), None);
        let namespace = tenant_object_namespace(&TenantId::new("tenant-a"))
            .expect("non-default tenant namespace");
        assert_eq!(namespace.len(), 64);
        assert_eq!(
            tenant_object_namespace(&TenantId::new("tenant-a")),
            Some(namespace)
        );
    }

    #[test]
    fn a_deployment_prefix_comes_before_the_tenant_namespace() {
        let other = TenantId::new("tenant-a");
        let namespace = tenant_object_namespace(&other).expect("namespace");
        assert_eq!(object_key_prefix(None, &TenantId::default()), None);
        assert_eq!(
            object_key_prefix(Some("/people/t1/a1/"), &TenantId::default()).as_deref(),
            Some("people/t1/a1")
        );
        assert_eq!(object_key_prefix(Some(""), &TenantId::default()), None);
        assert_eq!(
            object_key_prefix(None, &other),
            Some(format!("tenants/{namespace}"))
        );
        assert_eq!(
            object_key_prefix(Some("people/t1"), &other),
            Some(format!("people/t1/tenants/{namespace}"))
        );
    }

    #[tokio::test]
    async fn local_object_storage_is_namespaced_by_tenant() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut state = ServerState::new(
            ActorSystem::new("tenant-blob-isolation"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.path().to_path_buf();
        let tenant_a = TenantId::new("tenant-a");
        let tenant_b = TenantId::new("tenant-b");

        state
            .put_blob_object(&tenant_a, "field-overflow/value", b"tenant-a", None)
            .await
            .expect("tenant A write");
        state
            .put_blob_object(&tenant_b, "field-overflow/value", b"tenant-b", None)
            .await
            .expect("tenant B write");

        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant_a, "field-overflow/value")
                .await
                .expect("tenant A read"),
            Some(b"tenant-a".to_vec())
        );
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant_b, "field-overflow/value")
                .await
                .expect("tenant B read"),
            Some(b"tenant-b".to_vec())
        );
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&TenantId::new("tenant-c"), "field-overflow/value",)
                .await
                .expect("tenant C read"),
            None
        );
    }

    /// A Katagami app whose vault key was lost (Run probe, 21 Oct 2026) restored its data with no readable
    /// `blob_endpoint`, and a File put then answered 204 while its bytes went to this disk and the app's database.
    #[tokio::test]
    async fn a_server_that_keeps_files_only_in_an_object_store_refuses_them_without_one() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut state = ServerState::new(
            ActorSystem::new("files-need-object-store"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.path().to_path_buf();
        let tenant = TenantId::default();
        let key = "temper-fs/sha256:refused";

        state.files_need_object_store = true;
        let refused = state
            .put_file_content_blob(&tenant, key, b"bytes")
            .await
            .expect_err("no object store, so no File bytes");
        assert!(refused.contains("only in an object store"), "{refused}");
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant, key)
                .await
                .expect("read"),
            None,
            "nothing kept on disk or in the database"
        );

        // Without the setting, a local store keeps them as before.
        state.files_need_object_store = false;
        state
            .put_file_content_blob(&tenant, key, b"bytes")
            .await
            .expect("a local store keeps Files");
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&tenant, key)
                .await
                .expect("read"),
            Some(b"bytes".to_vec())
        );
    }
}
