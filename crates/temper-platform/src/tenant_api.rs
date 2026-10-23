//! REST API for tenant management.
//!
//! Routes:
//! - `POST   /api/tenants`              — create/provision a new tenant
//! - `GET    /api/tenants`              — list all tenants
//! - `DELETE /api/tenants/:id`          — remove a tenant
//! - `POST   /api/tenants/:id/users`    — add a user to a tenant
//! - `DELETE /api/tenants/:id/users/:user_id` — remove a user from a tenant
//! - `GET    /api/tenants/:id/users`    — list users for a tenant
//! - `GET    /api/genesis/apps/follow-updates` — list staged follow-latest rollout status

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing};

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use temper_authz::AuthenticatedRequestContext;
use temper_server::storage::TursoStoreProvider;

use crate::state::PlatformState;

mod apps;
mod auth;
mod users;
pub(crate) use apps::{
    get_genesis_app_bundle, get_os_app_guide, install_genesis_app, list_genesis_follow_updates,
    list_os_apps,
};
use auth::{
    PlatformResourceAuthorization, require_authenticated, require_control_plane,
    require_resource_authorization, require_same_tenant,
};
pub use users::{AddUserRequest, UserInfo};
use users::{add_user, list_users, remove_user};

/// Request body for `POST /api/tenants`.
#[derive(Debug, Deserialize)]
pub struct CreateTenantRequest {
    pub tenant_id: String,
}

/// Response body for tenant creation.
#[derive(Debug, Serialize)]
pub struct CreateTenantResponse {
    pub tenant_id: String,
    pub status: String,
}

/// Response body for tenant listing.
#[derive(Debug, Serialize)]
pub struct TenantListResponse {
    pub tenants: Vec<TenantInfo>,
}

/// Summary of a registered tenant.
#[derive(Debug, Serialize)]
pub struct TenantInfo {
    pub tenant_id: String,
    pub status: String,
}

fn turso_provider(state: &PlatformState) -> Option<Arc<dyn TursoStoreProvider>> {
    state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.turso.clone())
}

fn authorization_error(status: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({
            "error": if status == StatusCode::UNAUTHORIZED {
                "authentication required"
            } else {
                "authorization denied"
            }
        })),
    )
}

/// Build the tenant management API router.
pub fn tenant_api_router() -> Router<PlatformState> {
    Router::new()
        .route("/tenants", routing::post(create_tenant).get(list_tenants))
        .route("/tenants/{id}", routing::delete(delete_tenant))
        .route(
            "/tenants/{id}/users",
            routing::post(add_user).get(list_users),
        )
        .route(
            "/tenants/{id}/users/{user_id}",
            routing::delete(remove_user),
        )
        .route("/os-apps", routing::get(list_os_apps))
        .route("/os-apps/{name}", routing::get(get_os_app_guide))
        .route("/genesis/apps/install", routing::post(install_genesis_app))
        .route(
            "/genesis/apps/follow-updates",
            routing::get(list_genesis_follow_updates),
        )
        .route(
            "/genesis/apps/{owner}/{name}/versions/{hash}/bundle",
            routing::get(get_genesis_app_bundle),
        )
}

/// `POST /api/tenants` — provision a new tenant database.
async fn create_tenant(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<CreateTenantRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let tenant = match temper_runtime::tenant::TenantId::try_new(req.tenant_id.as_str()) {
        Ok(tenant) => tenant,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error })),
            );
        }
    };
    if let Err(status) = require_control_plane(authenticated).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "create_tenant",
                resource_type: "Tenant",
                resource_id: &req.tenant_id,
                attrs: std::collections::BTreeMap::from([(
                    "targetTenant".to_string(),
                    serde_json::Value::String(req.tenant_id.clone()),
                )]),
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

    match provider.register_tenant(&tenant).await {
        Ok(_store) => {
            // Bootstrap agent specs for the new tenant.
            // New tenant — no prior verification cache.
            crate::bootstrap_agent_specs(
                &state,
                &req.tenant_id,
                false,
                &std::collections::BTreeMap::new(),
            );
            (
                StatusCode::CREATED,
                Json(serde_json::json!(CreateTenantResponse {
                    tenant_id: req.tenant_id,
                    status: "active".to_string(),
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `GET /api/tenants` — list all registered tenants.
async fn list_tenants(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_control_plane(authenticated).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "list_tenants",
                resource_type: "TenantCatalog",
                resource_id: "all",
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
            StatusCode::OK,
            Json(serde_json::json!(TenantListResponse { tenants: vec![] })),
        );
    }

    match provider.list_tenants().await {
        Ok(ids) => {
            let tenants = ids
                .into_iter()
                .map(|id| TenantInfo {
                    tenant_id: id,
                    status: "active".to_string(),
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!(TenantListResponse { tenants })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `DELETE /api/tenants/:id` — remove a tenant and its data.
pub(crate) async fn delete_tenant(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
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

    // Remove the tenant's database, registry row and users, then what this process holds in memory for it, so an id
    // created again starts empty.
    // `require_same_tenant` checked that the path names the request's own tenant.
    let tid = authenticated.tenant();
    match provider.remove_tenant(tid).await {
        Ok(true) => {
            state.server.forget_tenant(tid);
            {
                let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
                registry.remove_tenant(tid);
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "deleted": true,
                    "tenant_id": tenant_id,
                })),
            )
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("tenant '{tenant_id}' not found")})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

#[cfg(test)]
mod tests;
