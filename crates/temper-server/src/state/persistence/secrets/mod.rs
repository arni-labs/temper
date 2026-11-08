//! Encrypted tenant secrets in the tenant's metadata store.

use std::collections::BTreeMap;

use temper_runtime::persistence::{SecretPrecondition, SecretWrite};

use super::super::ServerState;
use super::TenantMetadataBackend;

mod load;

pub use load::SecretLoadReport;

/// What a secret write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretWriteOutcome {
    /// The row was written and now carries this version id.
    Written { version_id: String },
    /// The key's current row did not meet the precondition; nothing changed.
    PreconditionFailed,
}

impl ServerState {
    /// Store an encrypted secret if the key's current row meets `precondition`.
    ///
    /// The store checks and writes in one statement. Each write gets a new
    /// version id from `sim_uuid` (a UUIDv7 in production, seeded under
    /// simulation); see [`SecretWrite::version_id`] for why it is never
    /// derived from the value.
    pub async fn write_secret(
        &self,
        tenant: &str,
        key_name: &str,
        ciphertext: &[u8],
        nonce: &[u8],
        precondition: SecretPrecondition<'_>,
    ) -> Result<SecretWriteOutcome, String> {
        let version_id = temper_runtime::scheduler::sim_uuid().simple().to_string();
        let write = SecretWrite {
            tenant,
            key_name,
            ciphertext,
            nonce,
            version_id: &version_id,
            precondition,
        };
        let written = match self.tenant_metadata_backend(tenant).await {
            // No store: the secret lives only in this server's memory, as
            // before. A condition has nothing durable to be checked against.
            None if precondition == SecretPrecondition::Any => true,
            None => {
                return Err(
                    "conditional secret writes are not supported without a metadata store".into(),
                );
            }
            Some(TenantMetadataBackend::Postgres(store)) => store
                .write_secret(&write)
                .await
                .map_err(|e| format!("failed to write secret {tenant}/{key_name}: {e}"))?,
            Some(TenantMetadataBackend::Turso(turso)) => turso
                .write_secret(&write)
                .await
                .map_err(|e| format!("failed to write secret {tenant}/{key_name} in turso: {e}"))?,
            Some(TenantMetadataBackend::Redis) => {
                return Err(Self::redis_ephemeral_error("Secret persistence"));
            }
        };
        Ok(if written {
            SecretWriteOutcome::Written { version_id }
        } else {
            SecretWriteOutcome::PreconditionFailed
        })
    }

    /// Delete a secret from the persistence backend.
    pub async fn delete_secret(&self, tenant: &str, key_name: &str) -> Result<bool, String> {
        match self.tenant_metadata_backend(tenant).await {
            None => Ok(false),
            Some(TenantMetadataBackend::Postgres(store)) => store
                .delete_secret(tenant, key_name)
                .await
                .map_err(|e| format!("failed to delete secret {tenant}/{key_name}: {e}")),
            Some(TenantMetadataBackend::Turso(turso)) => turso
                .delete_secret(tenant, key_name)
                .await
                .map_err(|e| format!("failed to delete secret {tenant}/{key_name} in turso: {e}")),
            Some(TenantMetadataBackend::Redis) => {
                Err(Self::redis_ephemeral_error("Secret deletion"))
            }
        }
    }

    /// The version id of each stored secret of a tenant, by name. Never reads
    /// a value. A row with no version id yet (written by an older release) is
    /// left out, as are platform secrets, which have no row.
    pub async fn secret_versions(&self, tenant: &str) -> Result<BTreeMap<String, String>, String> {
        let rows = match self.tenant_metadata_backend(tenant).await {
            // No store, or Redis, which refuses every secret write: no rows.
            None | Some(TenantMetadataBackend::Redis) => Vec::new(),
            Some(TenantMetadataBackend::Postgres(store)) => store
                .list_secret_versions(tenant)
                .await
                .map_err(|e| format!("failed to list secrets for tenant {tenant}: {e}"))?,
            Some(TenantMetadataBackend::Turso(turso)) => {
                turso.list_secret_versions(tenant).await.map_err(|e| {
                    format!("failed to list secrets for tenant {tenant} in turso: {e}")
                })?
            }
        };
        Ok(rows
            .into_iter()
            .filter_map(|(key_name, version_id)| version_id.map(|id| (key_name, id)))
            .collect())
    }
}

#[cfg(test)]
mod tests;
