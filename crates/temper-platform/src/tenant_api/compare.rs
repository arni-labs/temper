//! `GET /api/genesis/apps/{owner}/{name}/compare/{base}...{head}`: how two
//! versions of one app relate in its commit history.
//!
//! An installer asks this before it moves a dependency it already runs, so it
//! can tell an upgrade from a downgrade. It serves the same callers the bundle
//! route serves: anonymous ones for public apps, and authenticated ones as
//! Cedar allows.

use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing};
use temper_authz::AuthenticatedRequestContext;

use super::auth::{PlatformResourceAuthorization, require_resource_authorization};
use super::authorization_error;
use crate::genesis_install::BundleAudience;
use crate::genesis_install::version_compare::{
    GenesisVersionCompareResponse, VersionComparison, compare_genesis_app_versions,
    parse_compare_range,
};
use crate::state::PlatformState;

/// The compare route, for the tenant API router.
pub(super) fn routes() -> Router<PlatformState> {
    Router::new().route(
        "/genesis/apps/{owner}/{name}/compare/{range}",
        routing::get(compare_app_versions),
    )
}

/// Answer `{"status": "identical"|"ahead"|"behind"|"diverged"|"unknown"}`.
async fn compare_app_versions(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    Path((owner, name, range)): Path<(String, String, String)>,
) -> impl IntoResponse {
    let (base, head) = match parse_compare_range(&range) {
        Ok(range) => range,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": error })),
            );
        }
    };
    let Some(authenticated) = authenticated.as_deref() else {
        return anonymous_compare(&state, &headers, &owner, &name, &base, &head).await;
    };
    let registry_tenant = authenticated.tenant().as_str();
    // Reading how two versions relate tells less than reading either
    // version's bundle, so it is governed by the bundle's permission.
    let resource_id = format!("{owner}/{name}@{head}");
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_bundle",
            resource_type: "App",
            resource_id: &resource_id,
            attrs: std::collections::BTreeMap::from([
                (
                    "owner".to_string(),
                    serde_json::Value::String(owner.clone()),
                ),
                ("name".to_string(), serde_json::Value::String(name.clone())),
                (
                    "versionHash".to_string(),
                    serde_json::Value::String(head.clone()),
                ),
            ]),
        },
    ) {
        return authorization_error(status);
    }
    let compared = compare_genesis_app_versions(
        &state.server,
        registry_tenant,
        &owner,
        &name,
        &base,
        &head,
        BundleAudience::Authenticated,
    )
    .await;
    match compared {
        Ok(status) => compared_ok(status),
        Err(error) if error.contains("not found") || error.contains("no active Genesis App") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}

/// Compare for a caller with no credential, only within a public repository.
async fn anonymous_compare(
    state: &PlatformState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    base: &str,
    head: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let tenant = headers
        .get("x-tenant-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    if let Err(status) = super::auth::validate_tenant_id(tenant) {
        return authorization_error(status);
    }
    // One walk reads up to two bounded histories of Commit rows for a caller
    // the kernel cannot charge, so it takes a place in the same admission
    // the anonymous bundle export uses, and is refused rather than queued.
    let Ok(_permit) = super::apps::anonymous_bundle_admission().try_acquire() else {
        tracing::warn!(
            tenant,
            owner,
            name,
            "anonymous Genesis compare refused: at capacity"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "registry is busy; retry" })),
        );
    };
    let compared = compare_genesis_app_versions(
        &state.server,
        tenant,
        owner,
        name,
        base,
        head,
        BundleAudience::Anonymous,
    )
    .await;
    match compared {
        Ok(status) => compared_ok(status),
        Err(error) => {
            // One answer for every failure, as the bundle route gives: a
            // caller with no credential must not tell "private" from "absent".
            tracing::info!(
                tenant,
                owner,
                name,
                base,
                head,
                error,
                "anonymous Genesis compare refused"
            );
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "no public Genesis app versions at this reference"
                })),
            )
        }
    }
}

fn compared_ok(status: VersionComparison) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!(GenesisVersionCompareResponse { status })),
    )
}
