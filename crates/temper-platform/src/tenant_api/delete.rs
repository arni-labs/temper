//! Deleting a tenant and everything it holds.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_resource_authorization,
    require_same_tenant,
};
use super::{authorization_error, turso_provider};
use crate::state::PlatformState;

/// `DELETE /api/tenants/:id` — remove a tenant and its data: its database,
/// its objects in its object store, its blobs on this server's disk and
/// everything the server holds for it in memory, so a tenant created again
/// under the id starts empty.
pub(crate) async fn delete_tenant(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if temper_runtime::tenant::KEPT_TENANTS.contains(&tenant_id.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("tenant '{tenant_id}' holds the server's own data and is never deleted")
            })),
        );
    }
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "delete_tenant",
                resource_type: "Tenant",
                resource_id: &tenant_id,
                attrs: std::collections::BTreeMap::new(),
            },
        )
    }) {
        return authorization_error(status);
    }
    let Some(provider) = turso_provider(&state) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "no event store configured"})),
        );
    };

    if !provider.supports_tenant_admin() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "tenant management requires routed storage mode"})),
        );
    }

    // The registry row becomes a tombstone that keeps the id from being
    // created again until everything below is removed; any step that fails
    // leaves it, and deleting the tenant again finishes the removal.
    match provider.begin_tenant_removal(&tenant_id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("tenant '{tenant_id}' not found")})),
            );
        }
        Err(e) => return removal_failed(&tenant_id, e.to_string()),
    }
    let tid = temper_runtime::tenant::TenantId::new(&tenant_id);
    // Where its blobs are is read from its secrets, so before it is forgotten.
    let blobs = match state.server.tenant_blobs(&tid) {
        Ok(blobs) => blobs,
        Err(error) => return removal_failed(&tenant_id, error),
    };
    // Blobs first, while the tenant is whole, so a failure here can be
    // retried by the tenant; then the server forgets it, which stops its
    // actors; its blobs again, for any written meanwhile; then its database
    // and registry rows.
    if let Err(error) = blobs.remove().await {
        return removal_failed(&tenant_id, error);
    }
    if let Err(error) = state.server.forget_tenant(&tid).await {
        return removal_failed(&tenant_id, error);
    }
    if let Err(error) = blobs.remove().await {
        return removal_failed(&tenant_id, error);
    }
    match provider.remove_tenant(&tenant_id).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "deleted": true,
                "tenant_id": tenant_id,
            })),
        ),
        Err(e) => removal_failed(&tenant_id, e.to_string()),
    }
}

/// A removal that stopped part way: the tenant stays marked for removal and
/// its id cannot be created again until deleting it again finishes.
fn removal_failed(tenant_id: &str, reason: String) -> (StatusCode, Json<serde_json::Value>) {
    tracing::warn!(tenant_id, %reason, "tenant removal stopped; it stays marked for removal");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": format!(
                "tenant '{tenant_id}' was not fully removed and stays marked for removal; delete it again to finish: {reason}"
            )
        })),
    )
}
