//! The credential lookup completes before a blocked session lookup. Deletion and
//! recreation in that gap must never admit the old authority to the replacement.
use super::*;
use axum::{Router, middleware, routing};
use std::collections::BTreeMap;
use std::sync::Arc;
use temper_runtime::persistence::PersistenceError;
use temper_server::storage::TursoStoreProvider;
use temper_store_turso::{TenantUserRow, TursoEventStore};
use tower::ServiceExt;

#[derive(Default)]
struct PausedSessionLookup {
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

// Only metadata access is used in this fixture: identity entities use the real Sim journal and actors.
#[async_trait::async_trait]
impl TursoStoreProvider for PausedSessionLookup {
    async fn store_for_tenant(&self, _: &str) -> Option<TursoEventStore> {
        self.arrived.notify_one();
        self.release.notified().await;
        None
    }
    fn supports_tenant_admin(&self) -> bool {
        unreachable!()
    }
    fn platform_store(&self) -> Option<TursoEventStore> {
        unreachable!()
    }
    async fn all_stores(&self) -> Vec<TursoEventStore> {
        unreachable!()
    }
    async fn connected_tenants(&self) -> Vec<String> {
        unreachable!()
    }
    async fn tenants_for_user(&self, _: &str) -> Result<Vec<TenantUserRow>, PersistenceError> {
        unreachable!()
    }
    async fn register_tenant(&self, _: &TenantId) -> Result<TursoEventStore, PersistenceError> {
        unreachable!()
    }
    async fn list_tenants(&self) -> Result<Vec<String>, PersistenceError> {
        unreachable!()
    }
    async fn remove_tenant(&self, _: &TenantId) -> Result<bool, PersistenceError> {
        unreachable!()
    }
    async fn add_tenant_user(&self, _: &str, _: &str, _: &str) -> Result<(), PersistenceError> {
        unreachable!()
    }
    async fn list_tenant_users(&self, _: &str) -> Result<Vec<TenantUserRow>, PersistenceError> {
        unreachable!()
    }
    async fn remove_tenant_user(&self, _: &str, _: &str) -> Result<(), PersistenceError> {
        unreachable!()
    }
    async fn ensure_tenant(&self, _: &TenantId) -> Result<bool, PersistenceError> {
        unreachable!()
    }
}

#[tokio::test(start_paused = true)]
async fn dst_old_authority_cannot_lease_or_delete_a_replacement_generation() {
    for seed in 0..32 {
        let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
        let mut state = PlatformState::new(None);
        let tenant = TenantId::new("acme");
        state
            .server
            .set_storage_stack(temper_server::StorageStack::from_sim(
                temper_store_sim::SimEventStore::new(seed, Default::default()),
                None,
            ));
        crate::bootstrap::bootstrap_agent_specs(&state, "acme", false, &BTreeMap::new());
        crate::bootstrap::bootstrap_operator_credential(&state, "old-key", "acme")
            .await
            .unwrap();
        let original = state.server.tenant_generations.stamp(&tenant);
        let pause = Arc::new(PausedSessionLookup::default());
        let mut stack = state
            .server
            .storage_stack
            .as_ref()
            .unwrap()
            .as_ref()
            .clone();
        stack.turso = Some(pause.clone());
        state.server.set_storage_stack(stack);
        // A reached handler is a violation: the replacement's permissive policies could accept the old principal.
        let app = Router::new()
            .route("/protected", routing::get(|| async { StatusCode::OK }))
            .route(
                "/api/tenants/{id}",
                routing::delete(|| async { StatusCode::OK }),
            )
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_check,
            ));
        let healthy = Request::get("/protected")
            .header("authorization", "Bearer old-key")
            .header("x-tenant-id", "acme")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(healthy).await.unwrap().status(),
            StatusCode::OK,
            "seed {seed}: positive control before deletion"
        );
        let delete = seed % 2 == 1;
        let request = Request::builder()
            .method(if delete { "DELETE" } else { "GET" })
            .uri(if delete {
                "/api/tenants/acme"
            } else {
                "/protected"
            })
            .header("authorization", "Bearer old-key")
            .header("x-tenant-id", "acme")
            .header("x-session-id", "waiting-session")
            .body(axum::body::Body::empty())
            .unwrap();
        let replacement = async {
            pause.arrived.notified().await;
            for _ in 0..seed % 5 {
                tokio::task::yield_now().await;
            }
            state
                .server
                .remove_tenant(&original, || async { Ok(true) })
                .await
                .unwrap();
            crate::bootstrap::bootstrap_agent_specs(&state, "acme", false, &BTreeMap::new());
            let replacement = state.server.tenant_generations.stamp(&tenant);
            assert_ne!(original, replacement);
            state
                .server
                .authz
                .reload_tenant_policies("acme", "permit(principal, action, resource);")
                .unwrap();
            pause.release.notify_one();
            replacement
        };
        let (response, replacement) = tokio::join!(app.oneshot(request), replacement);
        assert_eq!(
            response.unwrap().status(),
            StatusCode::GONE,
            "seed {seed}, delete {delete}"
        );
        assert_eq!(state.server.tenant_generations.stamp(&tenant), replacement);
        assert!(
            state
                .server
                .tenant_generations
                .lease(&replacement)
                .is_some()
        );
    }
}
