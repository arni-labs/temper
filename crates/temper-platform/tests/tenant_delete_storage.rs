//! Deleting a tenant removes its storage: a tenant created again under the
//! same id starts empty, and nothing the deleted tenant held signs in to it.

use std::collections::BTreeMap;
use std::path::Path;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_platform::bootstrap::{bootstrap_operator_credential, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_platform::tenant_api::tenant_api_router;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::identity::IdentityResolver;
use temper_store_turso::TenantStoreRouter;
use tower::ServiceExt;

const TENANT: &str = "t";
const OPERATOR_KEY: &str = "tmpr_tenant-delete-storage";

/// A server in routed mode: one local database file per tenant.
async fn routed_state(dir: &Path) -> PlatformState {
    let router = TenantStoreRouter::new(
        &format!("file:{}", dir.join("platform.db").display()),
        None,
        Some(dir.join("tenants").display().to_string()),
    )
    .await
    .expect("tenant router");
    let mut state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    state
        .server
        .set_storage_stack(StorageStack::from_tenant_router(router));
    state
        .server
        .authz
        .reload_tenant_policies(
            "default",
            r#"permit(principal, action == Action::"create_tenant", resource);"#,
        )
        .expect("default may create tenants");
    state
}

fn as_tenant(method: Method, uri: &str, body: serde_json::Value, tenant: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request builds");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("agent-1", "operator", None),
        ));
    request
}

async fn create_tenant(state: &PlatformState) {
    let created = tenant_api_router()
        .with_state(state.clone())
        .oneshot(as_tenant(
            Method::POST,
            "/tenants",
            serde_json::json!({ "tenant_id": TENANT }),
            "default",
        ))
        .await
        .expect("create runs");
    assert_eq!(created.status(), StatusCode::CREATED);
}

async fn delete_tenant(state: &PlatformState) {
    state
        .server
        .authz
        .reload_tenant_policies(
            TENANT,
            r#"permit(principal, action == Action::"delete_tenant", resource);"#,
        )
        .expect("the tenant may delete itself");
    let deleted = tenant_api_router()
        .with_state(state.clone())
        .oneshot(as_tenant(
            Method::DELETE,
            &format!("/tenants/{TENANT}"),
            serde_json::Value::Null,
            TENANT,
        ))
        .await
        .expect("delete runs");
    assert_eq!(deleted.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_tenant_created_again_after_delete_starts_empty() {
    let dir = tempfile::tempdir().expect("data dir");
    let state = routed_state(dir.path()).await;
    let tenant = TenantId::new(TENANT);

    create_tenant(&state).await;
    bootstrap_operator_credential(&state, OPERATOR_KEY, TENANT)
        .await
        .expect("the tenant's credential and its type are written");
    assert_eq!(
        state
            .server
            .list_entity_ids_lazy(&tenant, "AgentType")
            .await,
        vec!["operator-type".to_string()]
    );
    assert!(
        IdentityResolver::new()
            .resolve(&state.server, &tenant, OPERATOR_KEY)
            .await
            .is_ok(),
        "the credential signs in before delete"
    );

    delete_tenant(&state).await;
    assert!(
        IdentityResolver::new()
            .resolve(&state.server, &tenant, OPERATOR_KEY)
            .await
            .is_err(),
        "the deleted tenant's credential does not sign in"
    );

    create_tenant(&state).await;
    assert_eq!(
        state
            .server
            .list_entity_ids_lazy(&tenant, "AgentType")
            .await,
        Vec::<String>::new(),
        "the tenant created again holds none of the deleted tenant's records"
    );
    assert!(
        IdentityResolver::new()
            .resolve(&state.server, &tenant, OPERATOR_KEY)
            .await
            .is_err(),
        "the deleted tenant's credential does not sign in to the tenant created again"
    );
}
