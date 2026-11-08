//! Encrypted tenant secrets in the tenant's metadata store.

use std::collections::BTreeMap;

use temper_runtime::persistence::{SecretPrecondition, SecretWrite};

use super::super::ServerState;
use super::TenantMetadataBackend;
use crate::secrets::vault::Opened;

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

    /// Load all secrets for a tenant from persistence, open, and cache.
    ///
    /// A row the current key opens is kept as stored. A row only the previous
    /// key opens is sealed again under the current key and written back on
    /// the condition that it still carries the version id read before it, so
    /// a write that landed meanwhile is never overwritten with an older value;
    /// the next load retries such a row. Once written back, a row opens with
    /// the current key, so loading again changes nothing. A row neither key
    /// opens is logged as an error and counted.
    pub async fn load_tenant_secrets(&self, tenant: &str) -> Result<SecretLoadReport, String> {
        let mut report = SecretLoadReport::default();
        let Some(vault) = self.secrets_vault.as_ref() else {
            return Ok(report);
        };
        // Versions before rows: a row rewritten after this read no longer
        // matches its version, so its re-seal is refused, not misapplied.
        let versions = if vault.has_previous_key() {
            self.secret_versions(tenant).await?
        } else {
            BTreeMap::new()
        };
        let rows = match self.tenant_metadata_backend(tenant).await {
            None => return Ok(report),
            Some(TenantMetadataBackend::Postgres(store)) => store
                .load_secrets_for_tenant(tenant)
                .await
                .map_err(|e| format!("failed to load secrets for tenant {tenant}: {e}"))?,
            Some(TenantMetadataBackend::Turso(turso)) => {
                turso.load_secrets_for_tenant(tenant).await.map_err(|e| {
                    format!("failed to load secrets for tenant {tenant} from turso: {e}")
                })?
            }
            Some(TenantMetadataBackend::Redis) => {
                return Err(Self::redis_ephemeral_error("Secret loading"));
            }
        };

        for (key_name, ciphertext, nonce) in &rows {
            let (plaintext, sealed_under_previous) = match vault.open(ciphertext, nonce) {
                Ok(Opened::Current(plaintext)) => (plaintext, false),
                Ok(Opened::Previous(plaintext)) => (plaintext, true),
                Err(e) => {
                    tracing::error!(
                        tenant,
                        key_name,
                        error = %e,
                        "secret cannot be opened with the vault keys; it is not loaded"
                    );
                    report.unreadable += 1;
                    continue;
                }
            };
            let value = String::from_utf8(plaintext)
                .map_err(|e| format!("secret {key_name} is not valid UTF-8: {e}"))?;
            if !sealed_under_previous {
                vault.cache_secret(tenant, key_name, value);
                report.kept += 1;
                continue;
            }
            let Some(version) = versions.get(key_name) else {
                // No version id to condition on: an older release wrote it.
                tracing::warn!(
                    tenant,
                    key_name,
                    "secret sealed under the previous key has no version id; re-seal deferred"
                );
                vault.cache_secret(tenant, key_name, value);
                report.deferred += 1;
                continue;
            };
            let (ciphertext, nonce) = vault.encrypt(value.as_bytes())?;
            match self
                .write_secret(
                    tenant,
                    key_name,
                    &ciphertext,
                    &nonce,
                    SecretPrecondition::Version(version),
                )
                .await?
            {
                SecretWriteOutcome::Written { .. } => {
                    vault.cache_secret(tenant, key_name, value);
                    report.resealed += 1;
                }
                SecretWriteOutcome::PreconditionFailed => {
                    // A newer write owns the row now; its writer caches it.
                    tracing::warn!(
                        tenant,
                        key_name,
                        "secret changed while being re-sealed; re-seal deferred"
                    );
                    report.deferred += 1;
                }
            }
        }
        debug_assert_eq!(report.total(), rows.len(), "every row is counted once");
        Ok(report)
    }
}

/// What loading a tenant's secrets did with each stored row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SecretLoadReport {
    /// Rows the current key opened, cached as stored.
    pub kept: usize,
    /// Rows only the previous key opened, sealed again under the current key.
    pub resealed: usize,
    /// Rows only the previous key opened that were not written back this time:
    /// the row changed during the load or has no version id. The next load
    /// retries them.
    pub deferred: usize,
    /// Rows neither key opened; not loaded.
    pub unreadable: usize,
}

impl SecretLoadReport {
    /// Rows counted, of any outcome.
    pub fn total(&self) -> usize {
        self.kept + self.resealed + self.deferred + self.unreadable
    }

    /// Add another tenant's counts to these.
    pub fn add(&mut self, other: SecretLoadReport) {
        self.kept += other.kept;
        self.resealed += other.resealed;
        self.deferred += other.deferred;
        self.unreadable += other.unreadable;
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_runtime::persistence::SecretPrecondition;
    use temper_store_turso::TursoEventStore;

    use super::{SecretLoadReport, SecretWriteOutcome};
    use crate::registry::SpecRegistry;
    use crate::secrets::vault::SecretsVault;
    use crate::state::ServerState;
    use crate::storage::StorageStack;

    fn make_state() -> ServerState {
        let system = ActorSystem::new("test-secrets-persistence");
        ServerState::from_registry(system, SpecRegistry::new())
            .with_secrets_vault(SecretsVault::new(&[7u8; 32]))
    }

    #[tokio::test]
    async fn turso_secret_round_trip() {
        let db_path =
            std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
        let db_url = format!("file:{}", db_path.display());
        let store = TursoEventStore::new(&db_url, None)
            .await
            .expect("create local turso db");

        let mut state = make_state();
        state.set_storage_stack(StorageStack::from_turso(store));

        let vault = state.secrets_vault.as_ref().expect("vault configured");
        let (ciphertext, nonce) = vault.encrypt(b"secret-value").expect("encrypt");

        let outcome = state
            .write_secret(
                "tenant-a",
                "API_KEY",
                &ciphertext,
                &nonce,
                SecretPrecondition::Any,
            )
            .await
            .expect("turso secret write should succeed");
        let SecretWriteOutcome::Written { version_id } = outcome else {
            panic!("an unconditional write is always written");
        };
        assert_eq!(
            state.secret_versions("tenant-a").await.unwrap()["API_KEY"],
            version_id
        );

        // Load and decrypt.
        let loaded = state
            .load_tenant_secrets("tenant-a")
            .await
            .expect("turso secret load should succeed");
        assert_eq!(loaded.kept, 1, "should have loaded 1 secret");

        // Verify the decrypted value is cached.
        let cached = vault
            .get_secret("tenant-a", "API_KEY")
            .expect("secret should be cached");
        assert_eq!(cached, "secret-value");

        // Delete secret.
        let deleted = state
            .delete_secret("tenant-a", "API_KEY")
            .await
            .expect("turso secret delete should succeed");
        assert!(deleted, "should have deleted 1 row");

        let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
    }

    async fn state_on(db_url: &str, vault: SecretsVault) -> ServerState {
        let store = TursoEventStore::new(db_url, None)
            .await
            .expect("open local turso db");
        let system = ActorSystem::new("test-secrets-rotation");
        let mut state =
            ServerState::from_registry(system, SpecRegistry::new()).with_secrets_vault(vault);
        state.set_storage_stack(StorageStack::from_turso(store));
        state
    }

    async fn seal_row(state: &ServerState, key_name: &str, value: &[u8]) {
        let vault = state.secrets_vault.as_ref().expect("vault configured");
        let (ciphertext, nonce) = vault.encrypt(value).expect("encrypt");
        state
            .write_secret("t", key_name, &ciphertext, &nonce, SecretPrecondition::Any)
            .await
            .expect("write secret");
    }

    #[tokio::test]
    async fn previous_key_rows_are_resealed_once_and_strays_are_counted() {
        let db_path =
            std::env::temp_dir().join(format!("temper-secrets-{}.db", uuid::Uuid::new_v4())); // determinism-ok: test-only temp file
        let db_url = format!("file:{}", db_path.display());
        let (old, new, stray) = ([1_u8; 32], [2_u8; 32], [3_u8; 32]);

        seal_row(
            &state_on(&db_url, SecretsVault::new(&old)).await,
            "OLD_A",
            b"a",
        )
        .await;
        seal_row(
            &state_on(&db_url, SecretsVault::new(&old)).await,
            "OLD_B",
            b"b",
        )
        .await;
        seal_row(
            &state_on(&db_url, SecretsVault::new(&stray)).await,
            "STRAY",
            b"s",
        )
        .await;
        seal_row(
            &state_on(&db_url, SecretsVault::new(&new)).await,
            "NEW",
            b"n",
        )
        .await;

        let rotated = state_on(&db_url, SecretsVault::new(&new).with_previous_key(&old)).await;
        let first = rotated.load_tenant_secrets("t").await.expect("first load");
        assert_eq!(
            first,
            SecretLoadReport {
                kept: 1,
                resealed: 2,
                deferred: 0,
                unreadable: 1
            }
        );
        let vault = rotated.secrets_vault.as_ref().unwrap();
        assert_eq!(vault.get_secret("t", "OLD_A").as_deref(), Some("a"));
        assert_eq!(vault.get_secret("t", "STRAY"), None);
        drop(rotated);

        // A second boot with both keys converges: nothing left to re-seal.
        let again = state_on(&db_url, SecretsVault::new(&new).with_previous_key(&old)).await;
        let second = again.load_tenant_secrets("t").await.expect("second load");
        assert_eq!(
            second,
            SecretLoadReport {
                kept: 3,
                resealed: 0,
                deferred: 0,
                unreadable: 1
            }
        );
        drop(again);

        // The previous key is no longer needed for the re-sealed rows.
        let after = state_on(&db_url, SecretsVault::new(&new)).await;
        let third = after
            .load_tenant_secrets("t")
            .await
            .expect("load without previous");
        assert_eq!(third.kept, 3);
        assert_eq!(
            after
                .secrets_vault
                .as_ref()
                .unwrap()
                .get_secret("t", "OLD_B")
                .as_deref(),
            Some("b")
        );

        let _ = std::fs::remove_file(db_path); // determinism-ok: test-only cleanup
    }
}
