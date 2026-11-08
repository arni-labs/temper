//! Loading a tenant's secrets at boot: opening each row with the vault's
//! keys and sealing rows still under the previous key again (ADR-0075).

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::persistence::SecretPrecondition;

use super::super::super::ServerState;
use super::super::TenantMetadataBackend;
use super::SecretWriteOutcome;
use crate::secrets::vault::{Opened, SecretsVault};

impl ServerState {
    /// Load all secrets for a tenant from persistence, open, and cache.
    ///
    /// A row the current key opens is kept as stored. A row only the previous
    /// key opens is sealed again under the current key and written back on
    /// the condition that it still carries the version id read before it, so
    /// a write that landed meanwhile is never overwritten with an older value.
    /// Such a changed row is read again and loaded as it now is. Once written
    /// back, a row opens with the current key, so loading again changes
    /// nothing. A re-seal that cannot be written leaves the row deferred, its
    /// value loaded, for the next load to retry. A row neither key opens is
    /// logged as an error and counted.
    pub async fn load_tenant_secrets(&self, tenant: &str) -> Result<SecretLoadReport, String> {
        let mut report = SecretLoadReport::default();
        let Some(vault) = self.secrets_vault.as_ref() else {
            return Ok(report);
        };
        // Versions before rows: a row rewritten after this read no longer
        // matches its version, so its re-seal is refused, not misapplied.
        let versions = if vault.has_previous_key() {
            self.secret_versions(tenant).await.unwrap_or_else(|e| {
                tracing::warn!(tenant, error = %e, "secret versions unreadable; re-seals deferred");
                BTreeMap::new()
            })
        } else {
            BTreeMap::new()
        };
        let Some(rows) = self.read_secret_rows(tenant).await? else {
            return Ok(report);
        };

        let mut changed = BTreeSet::new();
        for (key_name, ciphertext, nonce) in &rows {
            let Some((value, under_previous)) =
                open_row(vault, tenant, key_name, ciphertext, nonce)?
            else {
                report.unreadable += 1;
                continue;
            };
            if !under_previous {
                vault.cache_secret(tenant, key_name, value);
                report.kept += 1;
                continue;
            }
            match self
                .reseal(vault, tenant, key_name, &value, versions.get(key_name))
                .await
            {
                Reseal::Written => {
                    vault.cache_secret(tenant, key_name, value);
                    report.resealed += 1;
                }
                Reseal::Changed => {
                    changed.insert(key_name.clone());
                }
                Reseal::Deferred => {
                    vault.cache_secret(tenant, key_name, value);
                    report.deferred += 1;
                }
            }
        }
        if !changed.is_empty() {
            self.load_changed_rows(vault, tenant, &changed, &mut report)
                .await?;
        }
        debug_assert!(
            report.total() <= rows.len(),
            "a row is counted once at most"
        );
        Ok(report)
    }

    /// The tenant's stored secret rows, or `None` without a metadata store.
    async fn read_secret_rows(
        &self,
        tenant: &str,
    ) -> Result<Option<Vec<(String, Vec<u8>, Vec<u8>)>>, String> {
        Ok(Some(match self.tenant_metadata_backend(tenant).await {
            None => return Ok(None),
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
        }))
    }

    /// Seal `value` under the current key and write it back if the row still
    /// carries `version`.
    async fn reseal(
        &self,
        vault: &SecretsVault,
        tenant: &str,
        key_name: &str,
        value: &str,
        version: Option<&String>,
    ) -> Reseal {
        let Some(version) = version else {
            // No version id to condition on: an older release wrote it.
            tracing::warn!(
                tenant,
                key_name,
                "secret under the previous key has no version id; re-seal deferred"
            );
            return Reseal::Deferred;
        };
        let (ciphertext, nonce) = match vault.encrypt(value.as_bytes()) {
            Ok(sealed) => sealed,
            Err(e) => {
                tracing::warn!(tenant, key_name, error = %e, "secret re-seal failed; re-seal deferred");
                return Reseal::Deferred;
            }
        };
        let precondition = SecretPrecondition::Version(version);
        match self
            .write_secret(tenant, key_name, &ciphertext, &nonce, precondition)
            .await
        {
            Ok(SecretWriteOutcome::Written { .. }) => Reseal::Written,
            Ok(SecretWriteOutcome::PreconditionFailed) => Reseal::Changed,
            Err(e) => {
                tracing::warn!(tenant, key_name, error = %e, "secret re-seal not written; re-seal deferred");
                Reseal::Deferred
            }
        }
    }

    /// Load the rows in `changed` as they are now: another writer, perhaps
    /// another process, rewrote them while they were being re-sealed.
    async fn load_changed_rows(
        &self,
        vault: &SecretsVault,
        tenant: &str,
        changed: &BTreeSet<String>,
        report: &mut SecretLoadReport,
    ) -> Result<(), String> {
        let rows = match self.read_secret_rows(tenant).await {
            Ok(rows) => rows.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(tenant, error = %e, "changed secrets could not be read again; deferred");
                report.deferred += changed.len();
                return Ok(());
            }
        };
        let now = rows
            .iter()
            .filter(|(key_name, ..)| changed.contains(key_name));
        for (key_name, ciphertext, nonce) in now {
            match open_row(vault, tenant, key_name, ciphertext, nonce)? {
                None => report.unreadable += 1,
                Some((value, under_previous)) => {
                    vault.cache_secret(tenant, key_name, value);
                    if under_previous {
                        report.deferred += 1;
                    } else {
                        report.kept += 1;
                    }
                }
            }
        }
        Ok(())
    }
}

/// What a re-seal of one row did.
enum Reseal {
    /// Written back under the current key.
    Written,
    /// Refused: the row changed since its version was read.
    Changed,
    /// Not written (no version id, or the seal or the write failed); the
    /// next load retries it.
    Deferred,
}

/// Open a stored row with the vault's keys: its value and whether only the
/// previous key opened it, or `None`, logged as an error, if neither did.
fn open_row(
    vault: &SecretsVault,
    tenant: &str,
    key_name: &str,
    ciphertext: &[u8],
    nonce: &[u8],
) -> Result<Option<(String, bool)>, String> {
    let (plaintext, under_previous) = match vault.open(ciphertext, nonce) {
        Ok(Opened::Current(plaintext)) => (plaintext, false),
        Ok(Opened::Previous(plaintext)) => (plaintext, true),
        Err(e) => {
            tracing::error!(
                tenant,
                key_name,
                error = %e,
                "secret cannot be opened with the vault keys; it is not loaded"
            );
            return Ok(None);
        }
    };
    let value = String::from_utf8(plaintext)
        .map_err(|e| format!("secret {key_name} is not valid UTF-8: {e}"))?;
    Ok(Some((value, under_previous)))
}

/// What loading a tenant's secrets did with each stored row.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SecretLoadReport {
    /// Rows the current key opened, cached as stored.
    pub kept: usize,
    /// Rows only the previous key opened, sealed again under the current key.
    pub resealed: usize,
    /// Rows only the previous key opened that were not written back this
    /// time (no version id, a failed write, or rewritten during the load and
    /// still under the previous key), loaded as they are. The next load
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
