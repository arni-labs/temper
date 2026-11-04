//! Where one tenant's blobs are, so a removed tenant's can all be removed.

use std::path::PathBuf;

use temper_runtime::tenant::TenantId;

use super::BlobStore;
use super::state::{tenant_blob_root, tenant_object_namespace};
use crate::state::ServerState;

/// Where a tenant's blobs are: its namespace in its object store, if its
/// secrets name one, and its directories on this server's disk. Taken before
/// the tenant is forgotten, since its object store is named by its secrets,
/// and removable more than once.
pub struct TenantBlobs {
    tenant: TenantId,
    object_store: Option<BlobStore>,
    /// Its blobs on this server's disk, when the server keeps them there.
    local_dir: Option<PathBuf>,
    /// Uploads on their way to its object store.
    staging_dir: PathBuf,
}

impl ServerState {
    /// Where `tenant`'s blobs are. The default tenant's namespace is the
    /// store's root, which holds every other tenant's, so it is refused.
    pub fn tenant_blobs(&self, tenant: &TenantId) -> Result<TenantBlobs, String> {
        let Some(namespace) = tenant_object_namespace(tenant) else {
            return Err(format!(
                "tenant '{tenant}' keeps its blobs at the store's root, which is never removed"
            ));
        };
        let tenant_prefix = format!("tenants/{namespace}");
        let object_store = match self.blob_store_for_tenant(tenant) {
            Ok(store) if store.is_object_store() => {
                if !store
                    .object_key_prefix()
                    .is_some_and(|prefix| prefix.ends_with(&tenant_prefix))
                {
                    return Err(format!(
                        "tenant '{tenant}' object store prefix is not its namespace '{tenant_prefix}'"
                    ));
                }
                Some(store)
            }
            _ => None,
        };
        let blobs = TenantBlobs {
            tenant: tenant.clone(),
            object_store,
            local_dir: self
                .local_blob_root()
                .map(|root| tenant_blob_root(root, tenant)),
            staging_dir: self.blob_staging_root(tenant),
        };
        for dir in blobs.local_dirs() {
            assert!(
                dir.ends_with(&namespace),
                "{} is not a tenant namespace",
                dir.display()
            );
        }
        Ok(blobs)
    }
}

impl TenantBlobs {
    fn local_dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.local_dir.iter().chain([&self.staging_dir])
    }

    /// A blob an earlier tenant with this id left behind, which a new tenant
    /// must not be given: the first object under its namespace in its object
    /// store, or the first file in its directory on disk. Uploads still
    /// staging are not blobs anyone reads, so they are not counted. `None`
    /// when there is none.
    pub async fn left_over(&self) -> Result<Option<String>, String> {
        if let Some(store) = self.object_store.as_ref()
            && let Some(key) = store.first_object().await?
        {
            return Ok(Some(format!("object '{key}' in its object store")));
        }
        let Some(dir) = self.local_dir.as_ref() else {
            return Ok(None);
        };
        // determinism-ok: production blob store filesystem boundary
        let mut entries = match tokio::fs::read_dir(dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("failed to read {}: {error}", dir.display())),
        };
        match entries.next_entry().await {
            Ok(entry) => Ok(entry.map(|entry| format!("blob {}", entry.path().display()))),
            Err(error) => Err(format!("failed to read {}: {error}", dir.display())),
        }
    }

    /// Remove every blob of the tenant: each object under its namespace in
    /// its object store, then its directories on disk. A list or delete the
    /// object store refuses fails with the provider's reason, and what was
    /// not removed stays to be removed again.
    pub async fn remove(&self) -> Result<(), String> {
        let tenant = &self.tenant;
        if let Some(store) = self.object_store.as_ref() {
            let removed = store.remove_every_object().await.map_err(|error| {
                format!("tenant '{tenant}' objects were not all removed: {error}")
            })?;
            tracing::info!(%tenant, removed, "removed tenant objects from its object store");
        }
        for dir in self.local_dirs() {
            // determinism-ok: production blob store filesystem boundary
            match tokio::fs::remove_dir_all(dir).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "failed to remove tenant '{tenant}' blobs at {}: {error}",
                        dir.display()
                    ));
                }
            }
        }
        Ok(())
    }
}
