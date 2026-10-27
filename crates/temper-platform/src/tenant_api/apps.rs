use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_resource_authorization,
    require_same_tenant,
};
use super::authorization_error;
use crate::state::PlatformState;

/// List the credential tenant's authorized application catalog.
pub(crate) async fn list_os_apps(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_catalog",
            resource_type: "AppCatalog",
            resource_id: "all",
            attrs: std::collections::BTreeMap::new(),
        },
    ) {
        return authorization_error(status);
    }
    let apps = crate::os_apps::list_os_apps();
    (StatusCode::OK, Json(serde_json::json!({ "apps": apps })))
}

/// Return one authorized application guide.
pub(crate) async fn get_os_app_guide(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_catalog",
            resource_type: "AppCatalogEntry",
            resource_id: &name,
            attrs: std::collections::BTreeMap::new(),
        },
    ) {
        return authorization_error(status);
    }
    match crate::os_apps::get_app_guide(&name) {
        Some(guide) => (
            StatusCode::OK,
            Json(serde_json::json!({"name": name, "guide": guide})),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("No app guide found for '{name}'"),
            })),
        ),
    }
}

/// Return follow-latest state only for the credential-bound tenant.
pub(crate) async fn list_genesis_follow_updates(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let tenant = authenticated.tenant().as_str();
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_installs",
            resource_type: "AppInstall",
            resource_id: tenant,
            attrs: std::collections::BTreeMap::new(),
        },
    ) {
        return authorization_error(status);
    }
    let updates = crate::genesis_install::list_genesis_follow_latest_updates(&state)
        .await
        .into_iter()
        .filter(|update| update.tenant == tenant)
        .collect::<Vec<_>>();
    (
        StatusCode::OK,
        Json(serde_json::json!({ "value": updates })),
    )
}

/// Install one pinned application into the credential-bound tenant.
pub(crate) async fn install_genesis_app(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(req): Json<crate::genesis_install::GenesisRegistryInstallRequest>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(status) = require_same_tenant(authenticated, &req.tenant).and_then(|_| {
        require_resource_authorization(
            &state,
            authenticated,
            PlatformResourceAuthorization {
                action: "install_app",
                resource_type: "App",
                resource_id: &req.app_ref,
                attrs: std::collections::BTreeMap::from([
                    (
                        "targetTenant".to_string(),
                        serde_json::Value::String(req.tenant.clone()),
                    ),
                    (
                        "registryTenant".to_string(),
                        serde_json::Value::String(req.registry_tenant.clone()),
                    ),
                ]),
            },
        )
    }) {
        return authorization_error(status);
    }
    match crate::genesis_install::install_genesis_app_from_registry(&state, req).await {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))),
        Err(error) if error.contains("not found") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}

/// Export one pinned application bundle from the credential-bound registry tenant.
pub(crate) async fn get_genesis_app_bundle(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> impl IntoResponse {
    // A public repository's bundle is the same content Genesis already serves to
    // anonymous callers over `git clone`, just encoded differently, so requiring
    // a credential here and not there was inconsistent — and it made installing
    // a public app impossible, because the install client sends no credential at
    // all (it sends only `X-Tenant-Id`). Naming a tenant cannot escalate
    // anything on this path: the only rows it can reach are ones already
    // world-readable over git.
    let Some(authenticated) = authenticated.as_deref() else {
        let tenant = headers
            .get("x-tenant-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("default")
            .to_string();
        return match anonymous_public_bundle(&state, &tenant, &owner, &name, &hash).await {
            Ok(response) => response,
            Err(status) => authorization_error(status),
        };
    };
    let registry_tenant = authenticated.tenant().as_str();
    let resource_id = format!("{owner}/{name}@{hash}");
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
                    serde_json::Value::String(hash.clone()),
                ),
            ]),
        },
    ) {
        return authorization_error(status);
    }
    match crate::genesis_install::export_genesis_registry_bundle(
        &state,
        registry_tenant,
        &owner,
        &name,
        &hash,
    )
    .await
    {
        Ok(bundle) => (StatusCode::OK, Json(serde_json::json!(bundle))),
        Err(error) if error.contains("not found") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        ),
    }
}

/// Serve a bundle without a credential, only when the backing repository is
/// public. Anything else is refused as unauthenticated.
async fn anonymous_public_bundle(
    state: &PlatformState,
    tenant: &str,
    owner: &str,
    name: &str,
    hash: &str,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    // Validate the tenant name, then let the exporter decide what an anonymous
    // caller may see. The visibility question belongs there because the answer
    // is about the whole dependency closure, not about the app that was named:
    // an earlier version of this checked only the root repository, so a public
    // app that depended on a private one exported the private one too.
    super::auth::validate_tenant_id(tenant)?;

    // Admission bound. This path is reachable with no credential at all, and
    // exporting a bundle materializes a whole dependency closure — the most
    // memory an anonymous request can make this kernel spend. The per-bundle
    // budgets bound ONE export; nothing bounded how many run at once. A small
    // process-wide cap, refused rather than queued: an anonymous caller gets
    // 503 and can retry, and a burst cannot pile up materializations.
    let Ok(_permit) = anonymous_bundle_admission().try_acquire() else {
        tracing::warn!(
            tenant,
            owner,
            name,
            "anonymous Genesis bundle export refused: at capacity"
        );
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    match crate::genesis_install::export_genesis_registry_bundle_for(
        state,
        tenant,
        owner,
        name,
        hash,
        crate::genesis_install::BundleAudience::Anonymous,
    )
    .await
    {
        Ok(bundle) => Ok((StatusCode::OK, Json(serde_json::json!(bundle)))),
        Err(error) => {
            // One answer for every failure. A caller who presented no
            // credential must not be able to tell "this app is private" from
            // "this app does not exist" -- that difference is an existence
            // oracle over private repositories, and the reason to refuse is
            // exactly the reason not to explain. The detail is logged instead.
            tracing::info!(
                tenant,
                owner,
                name,
                hash,
                error,
                "anonymous Genesis bundle read refused"
            );
            Ok((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "no public Genesis app bundle at this reference"
                })),
            ))
        }
    }
}

/// How many anonymous registry reads (bundle exports and version compares)
/// may run at once, process-wide.
///
/// Deliberately small. Each export can materialize up to
/// `MAX_GENESIS_BUNDLE_APPS` app trees, each compare can read two bounded
/// commit histories, and the caller presented nothing that would let us charge
/// the cost to anyone. Authenticated reads are governed by Cedar per caller and
/// are not subject to this cap.
const MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS: usize = 4;

pub(super) fn anonymous_bundle_admission() -> &'static tokio::sync::Semaphore {
    static ADMISSION: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    ADMISSION.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS))
}

#[cfg(test)]
mod anonymous_admission_tests {
    use super::*;

    #[test]
    fn the_anonymous_bundle_cap_refuses_rather_than_queues_when_full() {
        let admission = anonymous_bundle_admission();
        let held: Vec<_> = (0..MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS)
            .map(|_| admission.try_acquire().expect("capacity available"))
            .collect();
        assert!(
            admission.try_acquire().is_err(),
            "the request past the cap must be refused, not queued behind a materialization"
        );
        drop(held);
        assert!(
            admission.try_acquire().is_ok(),
            "capacity returns when an export finishes"
        );
    }
}
