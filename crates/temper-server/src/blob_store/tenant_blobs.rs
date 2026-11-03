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
    local_dirs: Vec<PathBuf>,
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
        let local_dirs = self
            .local_blob_root()
            .map(|root| tenant_blob_root(root, tenant))
            .into_iter()
            .chain([self.blob_staging_root(tenant)])
            .collect::<Vec<_>>();
        for dir in &local_dirs {
            assert!(
                dir.ends_with(&namespace),
                "{} is not a tenant namespace",
                dir.display()
            );
        }
        Ok(TenantBlobs {
            tenant: tenant.clone(),
            object_store,
            local_dirs,
        })
    }
}

impl TenantBlobs {
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
        for dir in &self.local_dirs {
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
