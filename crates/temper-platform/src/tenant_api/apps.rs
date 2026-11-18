use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_authz::AuthenticatedRequestContext;
use temper_server::platform_store::InstalledAppRecord;

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_requested_resource_authorization,
    require_resource_authorization, require_same_tenant,
};
use super::authorization_error;
use crate::state::PlatformState;

/// List the credential tenant's authorized application catalog: the shared
/// apps and the tenant's own installs, never another tenant's.
pub(crate) async fn list_os_apps(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    if let Err(denial) = require_requested_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_catalog",
            resource_type: "AppCatalog",
            resource_id: "all",
            attrs: std::collections::BTreeMap::new(),
        },
    )
    .await
    {
        return denial;
    }
    let apps = crate::os_apps::list_tenant_os_apps(authenticated.tenant().as_str());
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
    if let Err(denial) = require_requested_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_catalog",
            resource_type: "AppCatalogEntry",
            resource_id: &name,
            attrs: std::collections::BTreeMap::new(),
        },
    )
    .await
    {
        return denial;
    }
    match crate::os_apps::tenant_app_guide(authenticated.tenant().as_str(), &name) {
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

/// One installed app and the version it runs, as `GET /genesis/apps/installs`
/// reports it.
#[derive(Debug, serde::Serialize)]
struct InstalledAppView {
    app_name: String,
    source_kind: String,
    app_ref: String,
    version_hash: String,
    pinned_version_hash: String,
    current_version_hash: String,
    follow_policy: String,
    status: String,
    installed_at: Option<String>,
    last_reconciled_at: Option<String>,
    /// The absolute path of the folder the tenant serves the app from (its
    /// `specs/`, its `COMMIT`), or `""` when only its stored specs serve.
    dir: String,
}

impl InstalledAppView {
    fn new(record: InstalledAppRecord, dir: String) -> Self {
        Self {
            app_name: record.app_name,
            source_kind: record.source_kind,
            app_ref: record.app_ref,
            version_hash: record.version_hash,
            pinned_version_hash: record.pinned_version_hash,
            current_version_hash: record.current_version_hash,
            follow_policy: record.follow_policy,
            status: record.status,
            installed_at: record.installed_at,
            last_reconciled_at: record.last_reconciled_at,
            dir,
        }
    }
}

/// The folder `tenant` serves `app` from, as an absolute path, or `""`
/// when no folder holds it. The response serializer escapes JSON characters.
fn served_dir(tenant: &str, app: &str) -> Result<String, String> {
    let Some(dir) = crate::os_apps::tenant_os_app_dir(tenant, app) else {
        return Ok(String::new());
    };
    let dir = std::path::absolute(&dir)
        .map_err(|error| format!("the folder of '{app}' has no absolute path: {error}"))?;
    let text = dir
        .to_str()
        .ok_or_else(|| format!("the folder of '{app}' ({}) is not UTF-8", dir.display()))?;
    Ok(text.to_string())
}

/// Authorize a read of the credential tenant's app installs and return that
/// tenant. The tenant comes from the credential alone: the install read routes
/// take no request input that could name another tenant.
fn authorize_app_install_read<'a>(
    state: &PlatformState,
    authenticated: Option<&'a AuthenticatedRequestContext>,
) -> Result<&'a str, StatusCode> {
    let authenticated = require_authenticated(authenticated)?;
    let tenant = authenticated.tenant().as_str();
    require_resource_authorization(
        state,
        authenticated,
        PlatformResourceAuthorization {
            action: "read_app_installs",
            resource_type: "AppInstall",
            resource_id: tenant,
            attrs: std::collections::BTreeMap::new(),
        },
    )?;
    Ok(tenant)
}

/// List every app installed in the credential's tenant with the version each
/// one runs, sorted by app name.
///
/// A release check uses this to prove production runs a merged app change: it
/// reads the installed version hashes instead of trusting that a deployed
/// image carried the commit. It is a plain read of the platform store and
/// makes no registry or network calls. A storage stack without a platform
/// store has nothing installed to report and lists no app.
///
/// Beside the apps (`value`), `installs` gives each app's last install this
/// process ran: `{app, id, app_ref, status}`, `status` being `running`,
/// `installed`, or `failed` with `reason`. A caller that got `202` from an
/// install polls this list for how it ended.
pub(crate) async fn list_app_installs(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let tenant = match authorize_app_install_read(&state, authenticated.as_deref()) {
        Ok(tenant) => tenant,
        Err(status) => return authorization_error(status),
    };
    match installed_apps_in_tenant(&state, tenant).await {
        Ok(apps) => {
            let installs: Vec<serde_json::Value> = state
                .installs
                .of_tenant(tenant)
                .into_iter()
                .map(|(app, run)| {
                    let mut entry = serde_json::json!(run);
                    entry["app"] = serde_json::Value::String(app);
                    entry
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::json!({ "value": apps, "installs": installs })),
            )
        }
        Err(error) => {
            tracing::warn!(tenant, error = %error, "failed to read installed apps");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "failed to read installed apps" })),
            )
        }
    }
}

/// Read one tenant's installed-app rows. A store failure is an error rather
/// than an empty list, so a caller never mistakes it for "nothing installed".
async fn installed_apps_in_tenant(
    state: &PlatformState,
    tenant: &str,
) -> Result<Vec<InstalledAppView>, String> {
    let mut apps = Vec::new();
    if let Some(platform_store) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        for (row_tenant, app_name) in platform_store.list_all_installed_apps().await? {
            if row_tenant != tenant {
                continue;
            }
            // None: the row has no metadata, e.g. it was removed between the reads.
            if let Some(record) = platform_store.get_installed_app(tenant, &app_name).await? {
                let dir = served_dir(tenant, &app_name)?;
                apps.push(InstalledAppView::new(record, dir));
            }
        }
    }
    apps.sort_by(|left, right| left.app_name.cmp(&right.app_name));
    Ok(apps)
}

/// Return follow-latest state only for the credential-bound tenant.
pub(crate) async fn list_genesis_follow_updates(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let tenant = match authorize_app_install_read(&state, authenticated.as_deref()) {
        Ok(tenant) => tenant,
        Err(status) => return authorization_error(status),
    };
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
    // A body cannot select another tenant, even through an approval request.
    if let Err(status) = require_same_tenant(authenticated, &req.tenant) {
        return authorization_error(status);
    }
    if let Err(denial) = require_requested_resource_authorization(
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
    .await
    {
        return denial;
    }
    // This server installs only from its own configured registry.
    if let Err(error) = crate::genesis_install::configured_registry(&req.registry_url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error })),
        );
    }
    // 200 with the install, 202 with its id while it runs on, or why not.
    let (status, body) = crate::genesis_install::install_answering(&state, req)
        .await
        .response();
    (status, Json(body))
}

/// Export one pinned application bundle from the credential-bound registry tenant.
pub(crate) async fn get_genesis_app_bundle(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    Path((owner, name, hash)): Path<(String, String, String)>,
) -> axum::response::Response {
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
            Err(status) => authorization_error(status).into_response(),
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
        return authorization_error(status).into_response();
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
        Ok(bundle) => (StatusCode::OK, Json(serde_json::json!(bundle))).into_response(),
        Err(error) if error.contains("not found") => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": error })),
        )
            .into_response(),
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
) -> Result<axum::response::Response, StatusCode> {
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
    // 503 and can retry, and a burst cannot pile up materializations. The
    // answer says it is busy and when to come back (`Retry-After`), which the
    // install client waits for; it is not a refusal of the caller.
    let Ok(_permit) = anonymous_bundle_admission().try_acquire() else {
        tracing::warn!(
            tenant,
            owner,
            name,
            "anonymous Genesis bundle export refused: at capacity"
        );
        let retry_after = BUSY_RETRY_AFTER_SECS.to_string();
        let error = "Genesis is exporting all the bundles it can at once; retry shortly";
        return Ok((
            StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, retry_after)],
            Json(serde_json::json!({ "error": error })),
        )
            .into_response());
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
        Ok(bundle) => Ok((StatusCode::OK, Json(serde_json::json!(bundle))).into_response()),
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
            )
                .into_response())
        }
    }
}

/// How many anonymous bundle exports may run at once, process-wide.
///
/// Deliberately small. Each export can materialize up to
/// `MAX_GENESIS_BUNDLE_APPS` app trees, and the caller presented nothing that
/// would let us charge the cost to anyone. Authenticated exports are governed
/// by Cedar per caller and are not subject to this cap.
const MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS: usize = 4;

fn anonymous_bundle_admission() -> &'static tokio::sync::Semaphore {
    static ADMISSION: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    ADMISSION.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS))
}

/// The seconds a busy anonymous export asks its caller to wait. An export
/// takes from under a second to about ten, so a caller asking again after one
/// finds a free place soon without asking much more often than they end.
const BUSY_RETRY_AFTER_SECS: u64 = 1;

#[cfg(test)]
#[path = "anonymous_bundle_test.rs"]
mod anonymous_bundle_test;
