//! Regressions from the pushed-install review: every dependency is checked
//! and verified, and filesystem paths round-trip through the JSON listing.

use super::*;
use crate::os_apps::{DirApps, add_tenant_os_apps_dir};

fn dependency(root: &Path, name: &str, dependencies: &[&str]) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("app.toml"),
        format!("name = {name:?}\ndependencies = {dependencies:?}\n"),
    )
    .unwrap();
    std::fs::write(dir.join("APP.md"), "# Install dependency").unwrap();
    std::fs::write(
        dir.join("model.csdl.xml"),
        app_files()["model.csdl.xml"].replace("SentinelMonitor", "PushDependency"),
    )
    .unwrap();
    std::fs::write(
        dir.join(SPEC),
        app_files()[SPEC].replace("SentinelMonitor", "PushDependency"),
    )
    .unwrap();
}

fn with_dependency(name: &str) -> BTreeMap<String, String> {
    let mut files = app_files();
    files
        .get_mut("app.toml")
        .unwrap()
        .push_str(&format!("\ndependencies = [{name:?}]\n"));
    files
}

#[tokio::test]
async fn a_transitive_dependency_fails_checks_before_any_install_changes() {
    let tenant = unique_tenant("closure-checks");
    let server = server(&tenant);
    let catalog = tempfile::tempdir().unwrap();
    dependency(catalog.path(), "push-parent", &["push-leaf"]);
    dependency(catalog.path(), "push-leaf", &[]);
    let path = catalog.path().join("push-leaf").join(SPEC);
    let spec = std::fs::read_to_string(&path).unwrap().replace(
        r#"effect = ["has_alerts = true", "alert_count += 1"]"#,
        r#"effect = ["alert_count += 1"]"#,
    );
    std::fs::write(path, spec).unwrap();
    add_tenant_os_apps_dir(&tenant, catalog.path().to_path_buf(), DirApps::Every);
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({
                "files": with_dependency("push-parent")
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        body["checks"][format!("push-leaf/{SPEC}")]["all_passed"],
        false,
        "{body}"
    );
    assert!(record(&server.ps, &tenant).await.is_none());
    assert!(
        server
            .ps
            .get_installed_app(&tenant, "push-parent")
            .await
            .unwrap()
            .is_none()
    );
    assert!(pushed_versions(server.data_dir.path()).is_empty());
}

#[tokio::test]
async fn a_dependency_with_broken_required_wasm_refuses_and_rolls_back() {
    let tenant = unique_tenant("closure-wasm");
    let server = server(&tenant);
    // Give the dependency its own pinned prior install, outside the root's
    // cache, so rollback must capture and recover its provenance as well.
    let mut request = push(
        &tenant,
        serde_json::json!({"files": {
            "app.toml": "name = \"push-wasm\"\n",
            "APP.md": "# Prior dependency",
            (SPEC): app_files()[SPEC].replace("SentinelMonitor", "PushDependency"),
            "model.csdl.xml": app_files()["model.csdl.xml"].replace("SentinelMonitor", "PushDependency")
        }}),
    );
    *request.uri_mut() = "/os-apps/push-wasm/install".parse().unwrap();
    let (status, body) = send(&server.state, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let prior_dependency = server
        .ps
        .get_installed_app(&tenant, "push-wasm")
        .await
        .unwrap()
        .unwrap();
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({
                "files": with_dependency("push-wasm")
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = listed_for(&server.state, &tenant).await;
    let catalog = tempfile::tempdir().unwrap();
    dependency(catalog.path(), "push-wasm", &[]);
    let dir = catalog.path().join("push-wasm");
    let mut manifest = std::fs::read_to_string(dir.join("app.toml")).unwrap();
    manifest.push_str(
        r#"
[[wasm_modules]]
name = "push_broken"
criticality = "app-required"
startup_loading = "lazy"
"#,
    );
    std::fs::write(dir.join("app.toml"), manifest).unwrap();
    std::fs::create_dir_all(dir.join("wasm/push_broken")).unwrap();
    std::fs::write(
        dir.join("wasm/push_broken/push_broken.wasm"),
        b"not a WASM module",
    )
    .unwrap();
    add_tenant_os_apps_dir(&tenant, catalog.path().to_path_buf(), DirApps::Every);
    let (status, body) = send(
        &server.state,
        push(
            &tenant,
            serde_json::json!({
                "files": with_dependency("push-wasm")
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("Rolled back"),
        "{body}"
    );
    assert_eq!(listed_for(&server.state, &tenant).await, before);
    let restored_dependency = server
        .ps
        .get_installed_app(&tenant, "push-wasm")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored_dependency.app_ref, prior_dependency.app_ref);
    assert_eq!(restored_dependency.source_kind, "pushed");
    assert_eq!(
        restored_dependency.bundle_digest,
        prior_dependency.bundle_digest
    );
    assert!(
        tenant_os_app(&tenant, "push-wasm")
            .unwrap()
            .wasm_modules
            .is_empty()
    );
}

#[tokio::test]
async fn listing_round_trips_a_directory_that_needs_json_escaping() {
    let tenant = unique_tenant("escaped-dir");
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("quote\"-backslash\\-brace}");
    std::fs::create_dir_all(&dir).unwrap();
    let ps = Arc::new(SimPlatformStore::new(0, SimPlatformFaultConfig::none()));
    let state = server_on(&tenant, &dir, &ps);
    let (status, body) = send(
        &state,
        push(&tenant, serde_json::json!({"files": app_files()})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = installs(&state, &tenant).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let entry = value["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["app_name"] == APP)
        .unwrap();
    let served = Path::new(entry["dir"].as_str().unwrap());
    assert!(served.starts_with(&dir), "{entry}");
    assert_eq!(
        std::fs::read_to_string(served.join(SPEC)).unwrap(),
        app_files()[SPEC]
    );
}
