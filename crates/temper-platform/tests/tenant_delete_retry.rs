//! Deleting a tenant that stops part way: its own requests are refused until the delete is finished, and the caller
//! that started it finishes it by deleting again, through the platform router on routed tenant storage.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_operator_credential};
use temper_platform::router::build_platform_router;
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_store_turso::TenantStoreRouter;
use tower::ServiceExt;

const KEY: &str = "tmpr_tenant-delete-retry";
const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// `acme` on routed local storage under `dir`, with an operator credential and a policy permitting everything.
async fn acme(dir: &std::path::Path) -> PlatformState {
    let router = TenantStoreRouter::new(
        &format!("file:{}", dir.join("platform.db").display()),
        None,
        Some(dir.join("tenants").to_string_lossy().to_string()),
    )
    .await
    .expect("tenant store router");
    router
        .register_tenant(&TenantId::new("acme"))
        .await
        .expect("acme provisioned");
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(StorageStack::from_tenant_router(router));
    // How long a delete waits for the tenant's work to finish before it stops and asks to be retried.
    state.server.action_dispatch_timeout = Duration::from_millis(200);
    bootstrap_agent_specs(&state, "acme", false, &BTreeMap::new());
    bootstrap_operator_credential(&state, KEY, "acme")
        .await
        .expect("operator credential");
    state
        .server
        .authz
        .reload_tenant_policies("acme", PERMIT_ALL)
        .expect("policy");
    state
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> StatusCode {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {KEY}"))
        .header("x-tenant-id", "acme")
        .header("content-type", "application/json");
    let body = body.map_or_else(Body::empty, |body| Body::from(body.to_string()));
    app.clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response")
        .status()
}

async fn put_policy(app: &Router) -> StatusCode {
    send(
        app,
        "PUT",
        "/api/tenants/acme/policies",
        Some(json!({ "policy_text": PERMIT_ALL })),
    )
    .await
}

#[tokio::test]
async fn a_delete_that_stops_part_way_refuses_the_tenants_writes_and_its_caller_finishes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let state = acme(dir.path()).await;
    let app = build_platform_router(state.clone());
    let database = dir.path().join("tenants").join("acme.db");
    assert_eq!(put_policy(&app).await, StatusCode::OK, "the tenant writes");

    // Work of the tenant that is still running when the delete starts: the delete waits for it, gives up and keeps
    // the tenant's storage, specs, credentials and policies.
    let running = state
        .server
        .tenant_generations
        .lease_live(&TenantId::new("acme"))
        .expect("acme is live");
    assert_eq!(
        send(&app, "DELETE", "/api/tenants/acme", None).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "the delete stops while the tenant's work runs"
    );
    assert!(database.exists(), "the tenant's storage is kept");

    // Until the delete is finished, the tenant's own requests are refused, writes included.
    assert_eq!(put_policy(&app).await, StatusCode::GONE);
    assert_eq!(
        send(&app, "GET", "/api/tenants/acme/policies", None).await,
        StatusCode::GONE
    );

    // The caller deletes again once the work has ended: the same credential is still accepted, and the delete
    // finishes.
    drop(running);
    assert_eq!(
        send(&app, "DELETE", "/api/tenants/acme", None).await,
        StatusCode::OK,
        "the retry is authorized and finishes the delete"
    );
    assert!(!database.exists(), "the tenant's storage is gone");
    assert_eq!(
        send(&app, "DELETE", "/api/tenants/acme", None).await,
        StatusCode::UNAUTHORIZED,
        "the deleted tenant's credential went with it"
    );
}
