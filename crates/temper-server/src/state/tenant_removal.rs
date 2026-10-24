//! Forgetting a deleted tenant's in-memory state.

use temper_runtime::tenant::TenantId;

use super::ServerState;

impl ServerState {
    /// Drop everything this process holds in memory for a deleted tenant: its live entity actors and their caches,
    /// its cached replies, its timer sequences, its HTTP routes, its Cedar policies, its cached secrets, its WASM
    /// modules and its specs. A tenant created again with the same id then starts from its own (empty) database, not
    /// from what the deleted tenant left in memory.
    ///
    /// Actors are stopped without the passivation snapshot, so nothing of the deleted tenant is written back.
    pub async fn forget_tenant(&self, tenant: &TenantId) {
        let prefix = format!("{tenant}:");
        let owned = |key: &String| key.starts_with(&prefix);

        if let Ok(mut registry) = self.actor_registry.write() {
            let keys: Vec<String> = registry.keys().filter(|key| owned(key)).cloned().collect();
            for key in keys {
                if let Some(actor) = registry.remove(&key) {
                    let _ = actor.stop();
                }
            }
        }
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.retain(|key, _| !owned(key));
        }
        if let Ok(mut cache) = self.entity_state_cache.lock() {
            let keys: Vec<String> = cache
                .iter()
                .map(|(key, _)| key)
                .filter(|key| owned(key))
                .cloned()
                .collect();
            for key in keys {
                cache.pop(&key);
            }
        }
        if let Ok(mut index) = self.entity_index.write() {
            index.retain(|key, _| !owned(key));
        }
        if let Ok(mut hydrated) = self.entity_index_hydrated.write() {
            hydrated.retain(|key| !owned(key));
        }
        if let Ok(mut backfilled) = self.key_index_backfilled.write() {
            backfilled.retain(|key, _| !owned(key));
        }
        if let Ok(mut loaded) = self.key_index_watermarks_loaded.write() {
            loaded.remove(tenant.as_str());
        }
        if let Ok(mut log) = self.entity_observe_log.lock() {
            log.retain(|key, _| !owned(key));
        }
        if let Ok(mut sequences) = self.entity_event_sequences.lock() {
            sequences.retain(|key, _| !owned(key));
        }

        self.idempotency_cache.forget_tenant(tenant);
        self.state_timeout_tracker.forget_tenant(tenant);
        self.http_endpoint_tables.remove(tenant).await;

        self.authz.remove_tenant(tenant.as_str());
        if let Ok(mut policies) = self.tenant_policies.write() {
            policies.remove(tenant.as_str());
        }
        if let Some(vault) = self.secrets_vault.as_ref() {
            vault.remove_tenant(tenant.as_str());
        }
        if let Ok(mut modules) = self.wasm_module_registry.write() {
            modules.remove_tenant(tenant);
        }
        if let Ok(mut registry) = self.registry.write() {
            registry.remove_tenant(tenant);
        }
        crate::runtime_metrics::record_server_state_metrics(self);
    }
}

#[cfg(test)]
mod tests {
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::CsdlDocument;

    use super::*;
    use crate::secrets::vault::SecretsVault;

    #[tokio::test]
    async fn a_deleted_tenants_policies_secrets_and_routes_are_forgotten_and_other_tenants_keep_theirs()
     {
        let state = ServerState::new(
            ActorSystem::new("forget-tenant"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        )
        .with_secrets_vault(SecretsVault::new(&[3_u8; 32]));
        let policy = r#"permit(principal, action, resource);"#;
        let vault = state.secrets_vault.clone().expect("vault");
        for tenant in ["acme", "other"] {
            state
                .authz
                .reload_tenant_policies(tenant, policy)
                .expect("policy");
            vault.cache_secret(tenant, "api_key", format!("{tenant}-value"));
            state
                .http_endpoint_tables
                .table_for(&TenantId::new(tenant))
                .await;
        }

        state.forget_tenant(&TenantId::new("acme")).await;

        assert_eq!(state.authz.get_tenant_policy_text("acme"), None);
        assert_eq!(vault.get_secret("acme", "api_key"), None);
        assert!(
            state
                .http_endpoint_tables
                .get(&TenantId::new("acme"))
                .await
                .is_none(),
            "the deleted tenant's routes are gone"
        );
        assert!(state.authz.get_tenant_policy_text("other").is_some());
        assert!(
            state
                .http_endpoint_tables
                .get(&TenantId::new("other"))
                .await
                .is_some()
        );
        assert_eq!(
            vault.get_secret("other", "api_key").as_deref(),
            Some("other-value")
        );
    }
}
