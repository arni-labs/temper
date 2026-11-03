//! What removing a tenant takes out of this server.
//!
//! The tenant's store removes its database (`TursoStoreProvider::remove_tenant`)
//! and [`ServerState::tenant_blobs`] its blobs; this forgets everything the server holds in memory for the tenant, so
//! a tenant created again under the same id starts from nothing.

use std::sync::PoisonError;

use temper_runtime::tenant::{KEPT_TENANTS, TenantId};
use tracing::instrument;

use super::ServerState;

impl ServerState {
    /// Forget `tenant`: stop its actors; drop its specs, index entries,
    /// cached statuses and replies, timer seqs, owed writes, observe events,
    /// Cedar policies, secrets, modules, routes and commons state. Every
    /// per-record key starts with
    /// `{tenant}:`, and a tenant id holds no colon, so no other tenant's entry
    /// matches. The tenants in [`KEPT_TENANTS`] are the server's own and are
    /// refused.
    #[instrument(skip_all, fields(otel.name = "state.forget_tenant", tenant = %tenant))]
    pub async fn forget_tenant(&self, tenant: &TenantId) -> Result<(), String> {
        if KEPT_TENANTS.contains(&tenant.as_str()) {
            return Err(format!(
                "tenant '{tenant}' holds the server's own data and is never removed"
            ));
        }
        self.registry
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove_tenant(tenant);

        let prefix = format!("{tenant}:");
        let theirs = |key: &String| key.starts_with(&prefix);
        let actors: Vec<_> = {
            let mut registry = self
                .actor_registry
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let keys: Vec<String> = registry.keys().filter(|key| theirs(key)).cloned().collect();
            keys.iter().filter_map(|key| registry.remove(key)).collect()
        };
        for actor in actors {
            let _ = actor.stop();
        }
        self.last_accessed
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        self.entity_index
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        self.entity_index_hydrated
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key| !theirs(key));
        self.key_index_backfilled
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        self.key_index_watermarks_loaded
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tenant.as_str());
        {
            let mut cache = self
                .entity_state_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let keys: Vec<String> = cache
                .iter()
                .map(|(key, _)| key)
                .filter(|key| theirs(key))
                .cloned()
                .collect();
            for key in keys {
                cache.pop(&key);
            }
        }
        self.idempotency_cache.forget_tenant(tenant);
        self.state_timeout_tracker.forget_tenant(tenant);
        self.owed_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, owed| &owed.tenant != tenant);
        self.entity_event_sequences
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        self.entity_observe_log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        self.agent_hints
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tenant);
        self.authz.remove_tenant(tenant.as_str());
        self.tenant_policies
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tenant.as_str());
        self.wasm_module_registry
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .forget_tenant(tenant);
        if let Some(vault) = self.secrets_vault.as_ref() {
            vault.forget_tenant(tenant.as_str());
        }
        self.http_endpoint_tables.forget_tenant(tenant).await;
        self.commons_guardrail_tenants
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tenant.as_str());
        self.commons_rate_limit_buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        self.commons_storage_projection_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|key, _| !theirs(key));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use temper_runtime::ActorSystem;
    use temper_runtime::tenant::TenantId;
    use temper_spec::csdl::CsdlDocument;

    use crate::secrets::vault::SecretsVault;
    use crate::state::ServerState;

    #[tokio::test]
    async fn forgetting_a_tenant_removes_only_what_is_its_own() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut state = ServerState::new(
            ActorSystem::new("forget-tenant"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.data_dir = data_dir.path().to_path_buf();
        state.secrets_vault = Some(Arc::new(SecretsVault::new(&[7u8; 32])));
        let (gone, kept) = (TenantId::new("tenant-a"), TenantId::new("tenant-b"));
        for tenant in [&TenantId::default(), &gone, &kept] {
            state
                .put_blob_object(tenant, "x", tenant.as_str().as_bytes(), None)
                .await
                .expect("blob");
            state.secrets_vault.as_ref().expect("vault").cache_secret(
                tenant.as_str(),
                "k",
                tenant.to_string(),
            );
            state
                .authz
                .reload_tenant_policies(
                    tenant.as_str(),
                    r#"permit(principal, action == Action::"read", resource);"#,
                )
                .expect("policy");
        }

        let blobs = state.tenant_blobs(&gone).expect("where its blobs are");
        state.forget_tenant(&gone).await.expect("forget");
        blobs.remove().await.expect("remove blobs");

        let vault = state.secrets_vault.as_ref().expect("vault");
        assert_eq!(
            state
                .get_blob_with_legacy_fallback(&gone, "x")
                .await
                .expect("read"),
            None
        );
        assert_eq!(vault.get_secret(gone.as_str(), "k"), None);
        assert_eq!(state.authz.get_tenant_policy_text(gone.as_str()), None);
        for tenant in [&TenantId::default(), &kept] {
            assert_eq!(
                state
                    .get_blob_with_legacy_fallback(tenant, "x")
                    .await
                    .expect("read"),
                Some(tenant.as_str().as_bytes().to_vec()),
                "{tenant}"
            );
            assert_eq!(
                vault.get_secret(tenant.as_str(), "k"),
                Some(tenant.to_string())
            );
            assert!(
                state
                    .authz
                    .get_tenant_policy_text(tenant.as_str())
                    .is_some()
            );
        }

        for own in ["default", "temper-system"] {
            assert!(
                state.forget_tenant(&TenantId::new(own)).await.is_err(),
                "{own}"
            );
        }
        assert!(state.tenant_blobs(&TenantId::default()).is_err());
        assert!(data_dir.path().join("blobs").join("x").exists());
    }
}
