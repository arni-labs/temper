//! Restoring an install reuses a cache directory only while it holds the recorded versions.

use std::sync::Arc;

use super::*;
use crate::os_apps::os_app_dir_bundle_digest;
use temper_server::platform_store::{SimPlatformFaultConfig, SimPlatformStore};

const TENANT: &str = "restore-test-tenant";

fn write_app(root: &Path, app_name: &str, marker: &str) {
    let app_dir = root.join(app_name);
    std::fs::create_dir_all(&app_dir).expect("app dir");
    std::fs::write(app_dir.join("app.toml"), format!("name = \"{app_name}\"\n")).expect("app.toml");
    std::fs::write(
        app_dir.join("APP.md"),
        format!("# {app_name}\n\n{marker}\n"),
    )
    .expect("APP.md");
}

/// `app_name`'s record as installed from `root` now.
fn recorded(root: &Path, app_name: &str) -> InstalledAppRecord {
    InstalledAppRecord {
        tenant: TENANT.to_string(),
        app_name: app_name.to_string(),
        source_kind: "genesis".to_string(),
        bundle_digest: os_app_dir_bundle_digest(root, app_name)
            .expect("digest")
            .bundle_digest,
        ..Default::default()
    }
}

fn store() -> SimPlatformStore {
    SimPlatformStore::new(0, SimPlatformFaultConfig::none())
}

#[tokio::test]
async fn a_cache_directory_is_reused_only_while_it_holds_the_recorded_version() {
    let root = tempfile::tempdir().expect("cache root");
    write_app(root.path(), "notes", "first version");
    let record = recorded(root.path(), "notes");
    let ps = store();
    let only = std::slice::from_ref(&record);
    assert!(stored_install_matches(&ps, root.path(), &record, only).await);

    write_app(root.path(), "notes", "another version");
    assert!(!stored_install_matches(&ps, root.path(), &record, only).await);
    assert!(!stored_install_matches(&ps, &root.path().join("gone"), &record, only).await);
}

#[tokio::test]
async fn a_cache_directory_whose_dependency_changed_is_not_reused() {
    let root = tempfile::tempdir().expect("cache root");
    write_app(root.path(), "notes", "first version");
    write_app(root.path(), "tags", "first version");
    let notes = recorded(root.path(), "notes");
    let ps = store();
    ps.record_installed_app_metadata(&recorded(root.path(), "tags"))
        .await
        .expect("record tags");
    let only = std::slice::from_ref(&notes);
    assert!(stored_install_matches(&ps, root.path(), &notes, only).await);

    write_app(root.path(), "tags", "another version");
    assert!(!stored_install_matches(&ps, root.path(), &notes, only).await);

    write_app(root.path(), "tags", "first version");
    write_app(root.path(), "unrecorded", "no record names it");
    assert!(!stored_install_matches(&ps, root.path(), &notes, only).await);
}

#[tokio::test]
async fn rollback_uses_the_cache_an_app_install_made() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.path().to_path_buf();
    let app_ref = format!("acme/notes@{}", "1".repeat(40));
    let local = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: TENANT,
            source: InstallSource::Local("default"),
        },
        &app_ref,
    );
    write_app(&local, "notes", "prior local version");
    let prior = InstalledAppRecord {
        app_ref,
        registry_tenant: "default".to_string(),
        ..recorded(&local, "notes")
    };

    ensure_prior_bundle_available(&platform, &store(), &prior, std::slice::from_ref(&prior))
        .await
        .expect("the prior bundle is available");

    let guide = crate::os_apps::tenant_app_guide(TENANT, "notes")
        .expect("the tenant resolves the prior notes");
    assert!(guide.contains("prior local version"), "{guide}");
}

/// One boot of a fresh server over `ps` and `data_dir`, restoring the stored
/// installs the way `temper serve` does: cache roots first, then a replay of
/// each stored install. Returns how many Genesis installs it restored.
async fn boot(data_dir: &Path, ps: &Arc<SimPlatformStore>) -> usize {
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.to_path_buf();
    platform
        .server
        .set_storage_stack(temper_server::StorageStack::from_sim(
            temper_store_sim::SimEventStore::no_faults(0),
            Some(ps.clone()),
        ));
    let restored = restore_genesis_registry_cache_roots(&platform).await;
    for (tenant, app_name) in ps.list_all_installed_apps().await.expect("installed apps") {
        crate::os_apps::restore_os_app(&platform, &tenant, &app_name)
            .await
            .expect("the stored install replays");
    }
    restored
}

#[tokio::test]
async fn a_genesis_install_is_restored_as_recorded_after_every_restart() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-restarts");
    let version = "2".repeat(40);
    let app_ref = format!("acme/notes@{version}");
    let mut platform = PlatformState::new(None);
    platform.server.data_dir = data_dir.path().to_path_buf();
    let cache = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: &tenant,
            source: InstallSource::Registry("default"),
        },
        &app_ref,
    );
    write_app(&cache, "notes", "registry version");
    let installed = InstalledAppRecord {
        tenant: tenant.clone(),
        app_ref: app_ref.clone(),
        version_hash: version.clone(),
        pinned_version_hash: version.clone(),
        current_version_hash: version.clone(),
        follow_policy: "pinned".to_string(),
        registry_url: "http://127.0.0.1:9".to_string(),
        registry_tenant: "default".to_string(),
        status: "installed".to_string(),
        ..recorded(&cache, "notes")
    };
    let ps = Arc::new(store());
    ps.record_installed_app_metadata(&installed)
        .await
        .expect("record the Genesis install");

    for restart in 1..=3 {
        assert_eq!(
            boot(data_dir.path(), &ps).await,
            1,
            "restart {restart} restores the Genesis install"
        );
        let stored = ps
            .get_installed_app(&tenant, "notes")
            .await
            .expect("read the install record")
            .expect("the install record is kept");
        assert_eq!(stored.source_kind, "genesis", "restart {restart}");
        assert_eq!(stored.app_ref, app_ref, "restart {restart}");
        assert_eq!(stored.version_hash, version, "restart {restart}");
        assert_eq!(
            stored.registry_url, installed.registry_url,
            "restart {restart}"
        );
        assert_eq!(stored.registry_tenant, "default", "restart {restart}");
        assert_eq!(
            stored.bundle_digest, installed.bundle_digest,
            "restart {restart}"
        );
    }
}
