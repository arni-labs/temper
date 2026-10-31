//! `POST /os-apps/{name}/install` as a caller sees it: who may push, what a
//! push may name, and what a refused or installed push leaves behind.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;
use tower::ServiceExt;

use crate::os_apps::{remove_tenant_os_apps_dir, restore_pushed_app_roots, tenant_os_app};
use crate::state::PlatformState;
use crate::tenant_api::tenant_api_router;

const APP: &str = "evolution";
const SPEC: &str = "sentinel_monitor.ioa.toml";

/// A server whose files live in their own directory, where `tenant` lets
/// any of its agents install apps.
fn server(tenant: &str) -> (PlatformState, tempfile::TempDir) {
    let data_dir = tempfile::tempdir().expect("data dir");
    let mut state = PlatformState::new(None);
    state.server.data_dir = data_dir.path().to_path_buf();
    state
        .server
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal, action == Action::"install_app", resource is App);"#,
        )
        .expect("install policy should parse");
    (state, data_dir)
}

fn unique_tenant(label: &str) -> String {
    format!("push-{label}-{}", uuid::Uuid::new_v4().simple())
}

/// Every file of the workspace's `evolution` app, as a push sends it.
fn app_files() -> BTreeMap<String, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../os-apps")
        .join(APP);
    let mut files = BTreeMap::new();
    let mut dirs = vec![root.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read app dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let relative = path.strip_prefix(&root).expect("inside app");
            let text = std::fs::read_to_string(&path).expect("app files are text");
            files.insert(relative.to_string_lossy().into_owned(), text);
        }
    }
    files
}

fn push(tenant: &str, body: serde_json::Value) -> Request<Body> {
    let mut request = Request::post(format!("/os-apps/{APP}/install"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity("agent-1", "operator", None),
        ));
    request
}

async fn send(state: &PlatformState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let response = tenant_api_router()
        .with_state(state.clone())
        .oneshot(request)
        .await
        .expect("request should run");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .expect("body");
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

fn home(state: &PlatformState, tenant: &str) -> PathBuf {
    state
        .server
        .data_dir
        .join("pushed-apps")
        .join(tenant)
        .join(APP)
}

fn current(state: &PlatformState, tenant: &str) -> Option<String> {
    std::fs::read_to_string(home(state, tenant).join("current")).ok()
}

/// The text of the spec `tenant` resolves for the app now.
fn resolved_spec(tenant: &str) -> String {
    let bundle = tenant_os_app(tenant, APP).expect("the tenant resolves the app");
    bundle
        .specs
        .into_iter()
        .find(|(entity_type, _)| entity_type == "SentinelMonitor")
        .map(|(_, source)| source)
        .expect("the app has its sentinel spec")
}

#[tokio::test]
async fn a_push_needs_a_credential_and_a_permit_in_its_own_tenant() {
    let tenant = unique_tenant("auth");
    let (state, _data_dir) = server(&tenant);
    let body = serde_json::json!({ "files": app_files() });

    let anonymous = Request::post(format!("/os-apps/{APP}/install"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request should build");
    assert_eq!(send(&state, anonymous).await.0, StatusCode::UNAUTHORIZED);

    // `tenant` permits installs; a credential of another tenant gets nothing
    // from that, and its push lands nowhere.
    let other = unique_tenant("other");
    assert_eq!(
        send(&state, push(&other, body)).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(!state.server.data_dir.join("pushed-apps").exists());
}

#[tokio::test]
async fn a_push_naming_a_path_outside_the_app_is_refused() {
    let tenant = unique_tenant("paths");
    let (state, _data_dir) = server(&tenant);
    for bad in ["../escape.txt", "/etc/escape", "specs//x.toml", "./APP.md"] {
        let mut files = app_files();
        files.insert(bad.to_string(), "x".to_string());
        let (status, body) =
            send(&state, push(&tenant, serde_json::json!({ "files": files }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
        assert!(
            body["error"].as_str().is_some_and(|e| e.contains(bad)),
            "{body}"
        );
    }
    assert!(!home(&state, &tenant).join("current").exists());
    assert!(!state.server.data_dir.join("../escape.txt").exists());
}

#[tokio::test]
async fn a_push_whose_spec_fails_its_check_changes_nothing() {
    let tenant = unique_tenant("refused");
    let (state, _data_dir) = server(&tenant);
    let (status, body) = send(
        &state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = current(&state, &tenant).expect("a good push is recorded");
    let spec_before = resolved_spec(&tenant);

    // Entering Triggering without setting has_alerts breaks the spec's own
    // invariant `Triggering => has_alerts`.
    let mut files = app_files();
    let spec = files.get_mut(SPEC).expect("sentinel spec");
    *spec = spec.replace(
        r#"effect = ["has_alerts = true", "alert_count += 1"]"#,
        r#"effect = ["alert_count += 1"]"#,
    );
    let (status, body) = send(&state, push(&tenant, serde_json::json!({ "files": files }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["checks"][SPEC]["all_passed"], false, "{body}");

    assert_eq!(current(&state, &tenant), Some(installed.clone()));
    assert_eq!(resolved_spec(&tenant), spec_before);
    let versions: Vec<_> = std::fs::read_dir(home(&state, &tenant))
        .expect("home")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "current")
        .collect();
    assert_eq!(versions, vec![installed], "the refused version is not kept");
}

/// A version whose checks pass is recorded as current before it is installed;
/// when the installer then refuses it, the previous version is current again.
#[tokio::test]
async fn a_push_the_installer_refuses_after_its_checks_leaves_the_previous_version_current() {
    let tenant = unique_tenant("install-refused");
    let (state, _data_dir) = server(&tenant);
    let (status, body) = send(
        &state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let installed = current(&state, &tenant).expect("a good push is recorded");
    let spec_before = resolved_spec(&tenant);

    // The specs pass their checks; the installer cannot parse the policy.
    let mut files = app_files();
    files.insert(
        "policies/evolution.cedar".to_string(),
        "permit(principal, action, resource) when { this is not cedar };".to_string(),
    );
    files
        .get_mut(SPEC)
        .expect("sentinel spec")
        .push_str("\n# refused by the installer\n");
    let (status, body) = send(&state, push(&tenant, serde_json::json!({ "files": files }))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().is_some_and(|e| e.contains("Cedar")),
        "{body}"
    );

    assert_eq!(current(&state, &tenant), Some(installed.clone()));
    assert_eq!(resolved_spec(&tenant), spec_before);
    let versions: Vec<_> = std::fs::read_dir(home(&state, &tenant))
        .expect("home")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "current")
        .collect();
    assert_eq!(versions, vec![installed], "the refused version is not kept");
}

#[tokio::test]
async fn a_good_push_is_installed_and_comes_back_after_a_restart() {
    let tenant = unique_tenant("installed");
    let (state, _data_dir) = server(&tenant);
    let mut files = app_files();
    files
        .get_mut(SPEC)
        .expect("sentinel spec")
        .push_str("\n# pushed to its own server\n");
    let (status, body) = send(&state, push(&tenant, serde_json::json!({ "files": files }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let digest = body["digest"].as_str().expect("digest").to_string();
    assert_eq!(body["installed"], APP);
    assert_eq!(body["checks"][SPEC]["all_passed"], true, "{body}");
    assert_eq!(current(&state, &tenant), Some(digest.clone()));
    assert!(resolved_spec(&tenant).contains("# pushed to its own server"));
    let registered = state
        .registry
        .read()
        .expect("registry")
        .get_spec(&TenantId::new(&tenant), "SentinelMonitor")
        .map(|spec| spec.ioa_source.clone())
        .expect("the pushed spec is registered");
    assert!(registered.contains("# pushed to its own server"));

    // A restart starts with no pushed version in the catalog; the one
    // installed last resolves again once it is restored.
    let root = home(&state, &tenant).join(&digest);
    remove_tenant_os_apps_dir(&tenant, &root);
    assert!(!resolved_spec(&tenant).contains("# pushed to its own server"));
    assert!(restore_pushed_app_roots(&state) >= 1);
    assert!(resolved_spec(&tenant).contains("# pushed to its own server"));

    // The same files again keep the unchanged spec by its hash.
    let mut kept = serde_json::to_value(&files).expect("files");
    let hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(files[SPEC].as_bytes())
    );
    kept[SPEC] = serde_json::json!({ "keep": hash });
    let (status, body) = send(&state, push(&tenant, serde_json::json!({ "files": kept }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["digest"], digest.as_str());
}

/// A push waits while another Temper process on this host holds the install
/// turn, and its answer says how long.
#[tokio::test]
async fn a_push_waits_for_the_host_install_turn() {
    let tenant = unique_tenant("turn");
    let (state, _data_dir) = server(&tenant);
    let path = std::env::temp_dir().join("temper-installs.lock");
    let held = tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .expect("lock file");
        file.lock().expect("take the turn");
        file
    })
    .await
    .expect("lock thread");
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(held);
    });

    let (status, body) = send(
        &state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    release.await.expect("the turn is given back");
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body["waited_ms"].as_u64().is_some_and(|ms| ms >= 400),
        "the push ran while the turn was held: {body}"
    );
}

/// Katagami's control plane reads these lines to know an app is served or
/// waiting; their words are fixed.
#[test]
fn the_install_and_waiting_lines_read_as_the_control_plane_reads_them() {
    let result = crate::os_apps::InstallResult {
        added: vec!["Order".to_string()],
        updated: vec!["Invoice".to_string()],
        skipped: vec!["Customer".to_string()],
        ..Default::default()
    };
    assert_eq!(
        crate::os_apps::installed_line("shop", "default", &result),
        "  App 'shop' installed for 'default': Order, Invoice, Customer"
    );
    assert_eq!(
        crate::os_apps::waiting_line("shop", "default"),
        "  App 'shop' waits for its first install for 'default'"
    );
    assert_eq!(
        crate::os_apps::stored_line("shop", "default"),
        "  App 'shop' serves its stored specs for 'default'; no files to install"
    );
}

/// An app served from the boot catalog is adopted as its first pushed
/// version; pushing the same files again is that version, and a changed push
/// replaces it.
#[tokio::test]
async fn an_adopted_catalog_version_is_the_current_one_a_push_builds_on() {
    let tenant = unique_tenant("adopted");
    let (state, _data_dir) = server(&tenant);
    let digest = crate::os_apps::adopt_catalog_app(&state, &tenant, APP)
        .expect("adopt")
        .expect("the catalog holds the app");
    assert_eq!(current(&state, &tenant), Some(digest.clone()));
    assert_eq!(
        crate::os_apps::adopt_catalog_app(&state, &tenant, APP).expect("adopt again"),
        None,
        "an app with a pushed version is not adopted again"
    );

    let (status, body) = send(
        &state,
        push(&tenant, serde_json::json!({ "files": app_files() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["digest"], digest.as_str());

    let mut files = app_files();
    files
        .get_mut(SPEC)
        .expect("sentinel spec")
        .push_str("\n# pushed after adoption\n");
    let (status, body) = send(&state, push(&tenant, serde_json::json!({ "files": files }))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_ne!(body["digest"], digest.as_str());
    assert!(resolved_spec(&tenant).contains("# pushed after adoption"));
}
