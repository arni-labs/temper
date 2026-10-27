//! Regression: another app's closure can hold the dependency version rollback needs.

use super::*;

fn cache(platform: &PlatformState, tenant: &str, app: &str, version: &str) -> PathBuf {
    genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant,
            source: InstallSource::Registry("default"),
        },
        &format!("acme/{app}@{version}"),
    )
}

fn record(root: &Path, tenant: &str, app: &str, version: &str) -> InstalledAppRecord {
    InstalledAppRecord {
        tenant: tenant.to_string(),
        app_ref: format!("acme/{app}@{version}"),
        version_hash: version.to_string(),
        registry_tenant: "default".to_string(),
        registry_url: "http://127.0.0.1:9".to_string(),
        status: "installed".to_string(),
        ..recorded(root, app)
    }
}

#[tokio::test]
async fn rollback_recovers_a_dependency_from_another_installed_closure() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-rollback-other-closure");
    let (v1, v2, v3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let notes_cache = cache(&platform, &tenant, "notes", &v1);
    write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
    write_app(&notes_cache, "tags", "tags v1");
    let other_cache = cache(&platform, &tenant, "other", &v2);
    write_app_needing(&other_cache, "other", "other v2", &["tags"]);
    write_app(&other_cache, "tags", "tags v2");
    let broken_cache = cache(&platform, &tenant, "notes", &v3);
    write_app_needing(&broken_cache, "notes", "notes v3", &["tags"]);
    write_app(&broken_cache, "tags", "broken tags v3");
    let notes = record(&notes_cache, &tenant, "notes", &v1);
    let tags = record(&other_cache, &tenant, "tags", &v2);
    let other = record(&other_cache, &tenant, "other", &v2);
    // The failed install has already replaced the selected bundles and records.
    for installed in [
        other,
        record(&broken_cache, &tenant, "notes", &v3),
        record(&broken_cache, &tenant, "tags", &v3),
    ] {
        ps.record_installed_app_metadata(&installed)
            .await
            .expect("record install");
    }
    register_install_cache(&tenant, broken_cache, DirApps::Every);
    assert!(!cache(&platform, &tenant, "tags", &v2).exists());
    crate::genesis_install::rollback_to_prior(
        &platform,
        ps.as_ref(),
        &tenant,
        &notes,
        &[notes.clone(), tags.clone()],
    )
    .await
    .expect("rollback recovers both recorded bundles offline");
    for (app, expected) in [("notes", "notes v1"), ("tags", "tags v2")] {
        let guide = crate::os_apps::tenant_app_guide(&tenant, app).expect("resolved app");
        assert!(guide.contains(expected), "{guide}");
    }
    assert_eq!(
        crate::os_apps::tenant_os_app_bundle_digest(&tenant, "tags")
            .expect("tags digest")
            .bundle_digest,
        tags.bundle_digest
    );
}

#[tokio::test]
async fn rollback_fails_when_the_recorded_dependency_cannot_be_recovered() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-rollback-missing");
    let (v1, v2, v3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
    let notes_cache = cache(&platform, &tenant, "notes", &v1);
    write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
    write_app(&notes_cache, "tags", "tags v1");
    let lost = tempfile::tempdir().expect("lost files");
    write_app(lost.path(), "tags", "tags v2");
    let notes = record(&notes_cache, &tenant, "notes", &v1);
    let tags = record(lost.path(), &tenant, "tags", &v2);
    let broken = cache(&platform, &tenant, "notes", &v3);
    write_app_needing(&broken, "notes", "notes v3", &["tags"]);
    write_app(&broken, "tags", "broken tags v3");
    let failed_notes = record(&broken, &tenant, "notes", &v3);
    ps.record_installed_app_metadata(&failed_notes)
        .await
        .expect("record failed root");
    ps.record_installed_app_metadata(&record(&broken, &tenant, "tags", &v3))
        .await
        .expect("record failed dependency");
    register_install_cache(&tenant, broken, DirApps::Every);
    // The only matching cache belongs to another tenant and must not be used.
    let foreign = cache(&platform, "another-tenant", "other", &v2);
    write_app(&foreign, "other", "other v2");
    write_app(&foreign, "tags", "tags v2");
    ps.record_installed_app_metadata(&record(&foreign, "another-tenant", "other", &v2))
        .await
        .expect("record foreign install");

    let error = crate::genesis_install::rollback_to_prior(
        &platform,
        ps.as_ref(),
        &tenant,
        &notes,
        &[notes.clone(), tags],
    )
    .await
    .expect_err("an unavailable prior dependency must fail rollback");
    assert!(error.contains("tags"), "{error}");
    assert_eq!(
        ps.get_installed_app(&tenant, "notes")
            .await
            .expect("read root")
            .expect("root record")
            .app_ref,
        failed_notes.app_ref
    );
}

#[tokio::test]
async fn rollback_fetches_the_recorded_dependency_and_rejects_changed_bytes() {
    use base64::Engine as _;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    for changed in [false, true] {
        let registry = MockServer::start().await;
        let data_dir = tempfile::tempdir().expect("data dir");
        let ps = Arc::new(store());
        let platform = server_on(data_dir.path(), &ps);
        let tenant = format!("{TENANT}-rollback-fetch-{changed}");
        let (v1, v2) = ("1".repeat(40), "2".repeat(40));
        let notes_cache = cache(&platform, &tenant, "notes", &v1);
        write_app_needing(&notes_cache, "notes", "notes v1", &["tags"]);
        write_app(&notes_cache, "tags", "tags v1");
        let remote = tempfile::tempdir().expect("registry files");
        write_app(remote.path(), "tags", "tags v2");
        let notes = record(&notes_cache, &tenant, "notes", &v1);
        let tags = InstalledAppRecord {
            registry_url: registry.uri(),
            ..record(remote.path(), &tenant, "tags", &v2)
        };
        if changed {
            write_app(remote.path(), "tags", "changed tags v2");
        }
        let files: Vec<_> = ["app.toml", "APP.md"]
            .into_iter()
            .map(|name| GenesisRegistryBundleFile {
                path: name.to_string(),
                content_base64: base64::engine::general_purpose::STANDARD.encode(
                    std::fs::read(remote.path().join("tags").join(name)).expect("bundle file"),
                ),
            })
            .collect();
        let bundle = GenesisRegistryBundleResponse {
            app_ref: tags.app_ref.clone(),
            registry_tenant: "default".to_string(),
            apps: vec![GenesisRegistryBundleApp {
                owner: "acme".to_string(),
                name: "tags".to_string(),
                version_hash: v2.clone(),
                files,
            }],
        };
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/genesis/apps/acme/tags/versions/{v2}/bundle"
            )))
            .and(header("X-Tenant-Id", "default"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&bundle))
            .expect(1)
            .mount(&registry)
            .await;
        let result = crate::genesis_install::rollback_to_prior(
            &platform,
            ps.as_ref(),
            &tenant,
            &notes,
            &[notes.clone(), tags.clone()],
        )
        .await;
        if changed {
            let error = result.expect_err("changed registry bytes must fail rollback");
            assert!(error.contains("recorded bundle"), "{error}");
            assert!(crate::os_apps::tenant_app_guide(&tenant, "tags").is_none());
        } else {
            result.expect("the recorded dependency is refetched");
            assert_eq!(
                crate::os_apps::tenant_os_app_bundle_digest(&tenant, "tags")
                    .expect("tags digest")
                    .bundle_digest,
                tags.bundle_digest
            );
        }
        registry.verify().await;
    }
}

#[tokio::test]
async fn rollback_checks_dependency_runtime_readiness() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-rollback-unready");
    let v1 = "1".repeat(40);
    let root = cache(&platform, &tenant, "notes", &v1);
    write_app_needing(&root, "notes", "notes v1", &["tags"]);
    write_app(&root, "tags", "tags v1");
    std::fs::write(
        root.join("tags/app.toml"),
        r#"name = "tags"
[[wasm_modules]]
name = "broken"
criticality = "app-required"
startup_loading = "lazy"
"#,
    )
    .expect("manifest");
    std::fs::create_dir_all(root.join("tags/wasm/broken")).expect("wasm dir");
    std::fs::write(root.join("tags/wasm/broken/broken.wasm"), b"not wasm").expect("broken wasm");
    let notes = record(&root, &tenant, "notes", &v1);
    let tags = record(&root, &tenant, "tags", &v1);
    let error = crate::genesis_install::rollback_to_prior(
        &platform,
        ps.as_ref(),
        &tenant,
        &notes,
        &[notes.clone(), tags],
    )
    .await
    .expect_err("a ready root cannot hide an unready dependency");
    assert!(error.contains("runtime-ready"), "{error}");
}
