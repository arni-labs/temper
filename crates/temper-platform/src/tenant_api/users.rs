//! Tenant user routes: add, list and remove the users of a tenant.

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use temper_authz::AuthenticatedRequestContext;

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_resource_authorization,
    require_same_tenant,
};
use super::{authorization_error, turso_provider};
use crate::state::PlatformState;

/// Request body for `POST /api/tenants/:id/users`.
#[derive(Debug, Deserialize)]
pub struct AddUserRequest {
    pub user_id: String,
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    "member".to_string()
}

/// Response body for user operations.
#[derive(Debug, Serialize)]
pub struct UserInfo {
    pub tenant_id: String,
    pub user_id: String,
    pub role: String,
}

/// `POST /api/tenants/:id/users` — add a user to a tenant.
pub(super) async fn add_user(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path(tenant_id): axum::extract::Path<String>,
    Json(req): Json<AddUserRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let user_resource_id = format!("{tenant_id}/{}", req.user_id);
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "manage_tenant_users",
                resource_type: "TenantUser",
                resource_id: &user_resource_id,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(tenant_id.clone()),
                    ),
                    (
                        "userId".to_string(),
                        serde_json::Value::String(req.user_id.clone()),
                    ),
                    (
                        "role".to_string(),
                        serde_json::Value::String(req.role.clone()),
                    ),
                ]),
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

    match provider
        .add_tenant_user(&tenant_id, &req.user_id, &req.role)
        .await
    {
        Ok(()) => (
            StatusCode::CREATED,
            Json(serde_json::json!(UserInfo {
                tenant_id,
                user_id: req.user_id,
                role: req.role,
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `GET /api/tenants/:id/users` — list users for a tenant.
pub(super) async fn list_users(
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
                action: "read_tenant_users",
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
        return (StatusCode::OK, Json(serde_json::json!({"users": []})));
    }

    match provider.list_tenant_users(&tenant_id).await {
        Ok(rows) => {
            let users: Vec<UserInfo> = rows
                .into_iter()
                .map(|r| UserInfo {
                    tenant_id: r.tenant_id,
                    user_id: r.user_id,
                    role: r.role,
                })
                .collect();
            (StatusCode::OK, Json(serde_json::json!({"users": users})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// `DELETE /api/tenants/:id/users/:user_id` — remove a user from a tenant.
pub(super) async fn remove_user(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    axum::extract::Path((tenant_id, user_id)): axum::extract::Path<(String, String)>,
) -> StatusCode {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status,
    };
    let resource_id = format!("{tenant_id}/{user_id}");
    if let Err(status) = require_same_tenant(authenticated, &tenant_id).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "manage_tenant_users",
                resource_type: "TenantUser",
                resource_id: &resource_id,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(tenant_id.clone()),
                    ),
                    (
                        "userId".to_string(),
                        serde_json::Value::String(user_id.clone()),
                    ),
                ]),
            },
        )
    }) {
        return status;
    }
    let Some(provider) = turso_provider(&state) else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };

    if !provider.supports_tenant_admin() {
        return StatusCode::BAD_REQUEST;
    }

    match provider.remove_tenant_user(&tenant_id, &user_id).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
