//! OData requests through the router, for tests that drive the HTTP surface.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, VerificationStatus};
use temper_server::{ServerState, build_router};
use tower::ServiceExt;

/// Mark an entity type verified so writes to it are served.
pub fn mark_verified(state: &ServerState, entity_type: &str) {
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        entity_type,
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-23T00:00:00Z".into(),
        }),
    );
}

/// Send one OData request through the router as `principal_id`, a
/// customer of the default tenant, and read the status and body.
pub async fn send(
    state: &ServerState,
    method: &str,
    path: &str,
    principal_id: &str,
    body: Option<Value>,
) -> (StatusCode, String) {
    let body = body.map_or_else(Body::empty, |value| Body::from(value.to_string()));
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext {
                principal: temper_authz::Principal {
                    id: principal_id.to_string(),
                    kind: temper_authz::PrincipalKind::Customer,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "create-existing-id-test".to_string(),
            },
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
