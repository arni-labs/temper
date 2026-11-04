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
use temper_server::secrets::vault::SecretsVault;
use temper_store_turso::TenantStoreRouter;
use tower::ServiceExt;

#[path = "tenant_delete_storage/object_store.rs"]
mod object_store;

use object_store::{BUCKET, ObjectStore};

const TENANT: &str = "t";
const OPERATOR_KEY: &str = "tmpr_tenant-delete-storage";

/// A server in routed mode: one local database file per tenant.
async fn routed_state(dir: &Path) -> PlatformState {
    routed_state_with_registry(dir, "platform.db").await
}

/// A server in routed mode whose tenant registry is `registry`, beside the
/// tenant databases in `dir/tenants`.
async fn routed_state_with_registry(dir: &Path, registry: &str) -> PlatformState {
    let router = TenantStoreRouter::new(
        &format!("file:{}", dir.join(registry).display()),
        None,
        Some(dir.join("tenants").display().to_string()),
    )
    .await
    .expect("tenant router");
    let mut state = PlatformState::new(None);
    state.server.secrets_vault = Some(std::sync::Arc::new(SecretsVault::new(&[3u8; 32])));
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

async fn create_tenant(state: &PlatformState) -> StatusCode {
    create_tenant_answer(state).await.0
}

/// Create the tenant: its status and what it said.
async fn create_tenant_answer(state: &PlatformState) -> (StatusCode, String) {
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
    let status = created.status();
    let body = axum::body::to_bytes(created.into_body(), 64 * 1024)
        .await
        .expect("create answer");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// Delete the tenant as itself: its status and what it said.
async fn delete_tenant(state: &PlatformState) -> (StatusCode, String) {
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
    let status = deleted.status();
    let body = axum::body::to_bytes(deleted.into_body(), 64 * 1024)
        .await
        .expect("delete answer");
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn a_tenant_created_again_after_delete_starts_empty() {
    let dir = tempfile::tempdir().expect("data dir");
    let state = routed_state(dir.path()).await;
    let tenant = TenantId::new(TENANT);

    assert_eq!(create_tenant(&state).await, StatusCode::CREATED);
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

    assert_eq!(delete_tenant(&state).await.0, StatusCode::OK);
    assert!(
        IdentityResolver::new()
            .resolve(&state.server, &tenant, OPERATOR_KEY)
            .await
            .is_err(),
        "the deleted tenant's credential does not sign in"
    );

    assert_eq!(create_tenant(&state).await, StatusCode::CREATED);
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

#[tokio::test]
async fn the_server_s_own_tenants_are_never_deleted() {
    let dir = tempfile::tempdir().expect("data dir");
    let state = routed_state(dir.path()).await;
    for own in ["default", "temper-system"] {
        state
            .server
            .authz
            .reload_tenant_policies(
                own,
                r#"permit(principal, action == Action::"delete_tenant", resource);"#,
            )
            .expect("policy");
        let refused = tenant_api_router()
            .with_state(state.clone())
            .oneshot(as_tenant(
                Method::DELETE,
                &format!("/tenants/{own}"),
                serde_json::Value::Null,
                own,
            ))
            .await
            .expect("delete runs");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{own}");
    }
    assert!(dir.path().join("platform.db").exists());
    assert!(
        state
            .registry
            .read()
            .expect("registry")
            .get_tenant(&TenantId::new("temper-system"))
            .is_some(),
        "the system tenant's specs stay"
    );
}

/// The tenant's namespace in its object store: `tenants/<sha256 of its id>/`.
fn object_namespace(tenant: &str) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(tenant.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("tenants/{hex}/")
}

#[tokio::test]
async fn deleting_a_tenant_removes_its_objects_and_a_refused_removal_keeps_the_id() {
    let dir = tempfile::tempdir().expect("data dir");
    let state = routed_state(dir.path()).await;
    let objects = ObjectStore::default();
    let server = objects.serve().await;
    let namespace = object_namespace(TENANT);
    let other = format!("{}x", object_namespace("other"));
    // More than one DeleteObjects batch, beside another tenant's object and
    // the default tenant's at the bucket's root.
    for n in 0..1001 {
        objects.put(&format!("{namespace}field-overflow/{n:04}"));
    }
    objects.put(&other);
    objects.put("x");

    assert_eq!(create_tenant(&state).await, StatusCode::CREATED);
    let vault = state.server.secrets_vault.as_ref().expect("vault");
    vault.cache_secret(TENANT, "blob_endpoint", server.uri());
    vault.cache_secret(TENANT, "blob_bucket", BUCKET.to_string());
    // Keys the requests are signed with; the store in memory does not check them.
    vault.cache_secret(TENANT, "blob_access_key", "access".to_string());
    vault.cache_secret(TENANT, "blob_secret_key", "secret".to_string());

    objects
        .refuse_deletes
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, said) = delete_tenant(&state).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{said}");
    assert!(
        said.contains("403"),
        "the refusal names the provider's answer: {said}"
    );
    assert_eq!(
        objects.keys_under(&namespace),
        1001,
        "nothing counts as removed"
    );
    assert_ne!(
        create_tenant(&state).await,
        StatusCode::CREATED,
        "the id is not created again while its removal is unfinished"
    );

    objects
        .refuse_deletes
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (status, said) = delete_tenant(&state).await;
    assert_eq!(status, StatusCode::OK, "deleting again finishes it: {said}");
    assert_eq!(objects.keys_under(&namespace), 0);
    assert!(
        objects.holds(&other) && objects.holds("x"),
        "only its own objects go"
    );
    assert_eq!(
        objects
            .delete_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2,
        "1,001 objects take two DeleteObjects calls"
    );
    assert_eq!(create_tenant(&state).await, StatusCode::CREATED);
}

#[tokio::test]
async fn a_tenant_id_whose_database_was_left_behind_is_refused_rather_than_reopened() {
    let dir = tempfile::tempdir().expect("data dir");
    // The tenant's database stays in the tenants directory while no registry
    // names it, as a delete that removed only the registry row left it.
    {
        let earlier = routed_state_with_registry(dir.path(), "earlier.db").await;
        assert_eq!(create_tenant(&earlier).await, StatusCode::CREATED);
        bootstrap_operator_credential(&earlier, OPERATOR_KEY, TENANT)
            .await
            .expect("the tenant's credential and its type are written");
    }
    let left_behind = dir.path().join("tenants").join(format!("{TENANT}.db"));
    assert!(left_behind.exists());

    let state = routed_state(dir.path()).await;
    let (status, said) = create_tenant_answer(&state).await;
    assert_eq!(status, StatusCode::CONFLICT, "{said}");
    assert!(
        said.contains(&format!("{TENANT}.db")),
        "names what is left: {said}"
    );
    assert!(left_behind.exists(), "what is left is not removed");
    assert!(
        IdentityResolver::new()
            .resolve(&state.server, &TenantId::new(TENANT), OPERATOR_KEY)
            .await
            .is_err(),
        "nothing left behind signs in"
    );
    let (status, said) = create_tenant_answer(&state).await;
    assert_eq!(status, StatusCode::CONFLICT, "refused again: {said}");
}

#[tokio::test]
async fn a_tenant_id_whose_objects_were_left_behind_is_refused() {
    let dir = tempfile::tempdir().expect("data dir");
    let state = routed_state(dir.path()).await;
    let objects = ObjectStore::default();
    let server = objects.serve().await;
    let left_behind = format!("{}field-overflow/0001", object_namespace(TENANT));
    objects.put(&left_behind);
    let vault = state.server.secrets_vault.as_ref().expect("vault");
    vault.cache_platform_secret("blob_endpoint", server.uri());
    vault.cache_platform_secret("blob_bucket", BUCKET.to_string());

    let (status, said) = create_tenant_answer(&state).await;
    assert_eq!(status, StatusCode::CONFLICT, "{said}");
    assert!(said.contains(&left_behind), "names what is left: {said}");
    assert!(objects.holds(&left_behind), "what is left is not removed");
}
