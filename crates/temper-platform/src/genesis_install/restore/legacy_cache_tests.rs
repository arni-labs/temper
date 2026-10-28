//! Upgrade recovery must recognize the cache layout used before tenant ownership.

use super::rollback_tests::{cache, record};
use super::*;

// Reproduce the old binary's layout independently of the recovery path.
fn old_cache(platform: &PlatformState, app: &str, version: &str) -> PathBuf {
    use sha2::Digest as _;
    let app_ref = format!("acme/{app}@{version}");
    let digest = format!("{:x}", sha2::Sha256::digest(app_ref.as_bytes()));
    platform
        .server
        .data_dir
        .join("genesis-app-cache")
        .join(format!("acme-{app}-{version}-{}", &digest[..16]))
}

#[tokio::test]
async fn legacy_cache_boot_copies_verified_apps_into_each_tenants_directory() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let v1 = "1".repeat(40);
    let old = old_cache(&platform, "notes", &v1);
    write_app(&old, "notes", "prior notes");
    std::fs::create_dir_all(old.join("notes/resources/nested")).unwrap();
    std::fs::write(old.join("notes/resources/nested/data.bin"), [0, 255, 42]).unwrap();
    for suffix in ["one", "two"] {
        let tenant = format!("{TENANT}-legacy-boot-{suffix}");
        ps.record_installed_app_metadata(&record(&old, &tenant, "notes", &v1))
            .await
            .unwrap();
    }
    assert_eq!(
        boot(data_dir.path(), &ps).await,
        2,
        "both tenants recover offline"
    );
    let mut copies = Vec::new();
    for suffix in ["one", "two"] {
        let tenant = format!("{TENANT}-legacy-boot-{suffix}");
        let destination = cache(&platform, &tenant, "notes", &v1);
        assert_eq!(
            std::fs::read(destination.join("notes/resources/nested/data.bin")).unwrap(),
            [0, 255, 42]
        );
        assert!(
            crate::os_apps::tenant_app_guide(&tenant, "notes")
                .unwrap()
                .contains("prior notes")
        );
        copies.push(destination);
    }
    assert_ne!(copies[0], copies[1]);
    write_app(&copies[0], "notes", "one tenant changes its files");
    assert!(
        crate::os_apps::tenant_app_guide(&format!("{TENANT}-legacy-boot-two"), "notes")
            .unwrap()
            .contains("prior notes")
    );
    assert!(
        std::fs::read_to_string(old.join("notes/APP.md"))
            .unwrap()
            .contains("prior notes")
    );
    write_app(&copies[0], "notes", "prior notes");
    std::fs::write(
        copies[0].join("notes/resources/nested/data.bin"),
        [0, 255, 42],
    )
    .unwrap();
    std::fs::remove_dir_all(&old).unwrap();
    assert_eq!(
        boot(data_dir.path(), &ps).await,
        2,
        "both tenant copies survive removal of the old cache"
    );
}

#[tokio::test]
async fn legacy_cache_rollback_recovers_root_and_dependency_from_another_closure() {
    for seed in 0..4 {
        let data_dir = tempfile::tempdir().expect("data dir");
        let ps = Arc::new(SimPlatformStore::new(seed, SimPlatformFaultConfig::none()));
        let platform = server_on(data_dir.path(), &ps);
        let tenant = format!("{TENANT}-legacy-rollback-{seed}");
        let (v1, v2, v3) = ("1".repeat(40), "2".repeat(40), "3".repeat(40));
        let old = old_cache(&platform, "notes", &v1);
        write_app_needing(&old, "notes", "notes v1", &["tags", "labels"]);
        write_app(&old, "tags", "tags v1");
        let other = old_cache(&platform, "other", &v2);
        write_app(&other, "other", "other v2");
        write_app(&other, "labels", "labels v2");
        let notes = record(&old, &tenant, "notes", &v1);
        let tags = record(&old, &tenant, "tags", &v1);
        let labels = record(&other, &tenant, "labels", &v2);
        ps.record_installed_app_metadata(&record(&other, &tenant, "other", &v2))
            .await
            .unwrap();
        let broken = cache(&platform, &tenant, "notes", &v3);
        for app in ["notes", "tags", "labels"] {
            write_app(&broken, app, "failed upgrade");
            ps.record_installed_app_metadata(&record(&broken, &tenant, app, &v3))
                .await
                .unwrap();
        }
        register_install_cache(&tenant, broken, DirApps::Every);
        let mut prior = vec![notes.clone(), tags.clone(), labels.clone()];
        if seed & 1 != 0 {
            prior.swap(1, 2);
        }
        crate::genesis_install::rollback_to_prior(&platform, ps.as_ref(), &tenant, &notes, &prior)
            .await
            .expect("old root and dependency caches recover offline");
        for (installed, destination) in [
            (&notes, cache(&platform, &tenant, "notes", &v1)),
            (&tags, cache(&platform, &tenant, "notes", &v1)),
            (&labels, cache(&platform, &tenant, "labels", &v2)),
        ] {
            assert_eq!(
                os_app_dir_bundle_digest(&destination, &installed.app_name)
                    .unwrap()
                    .bundle_digest,
                installed.bundle_digest
            );
            assert_eq!(
                ps.get_installed_app(&tenant, &installed.app_name)
                    .await
                    .unwrap()
                    .unwrap(),
                *installed
            );
        }
        std::fs::remove_dir_all(old).unwrap();
        std::fs::remove_dir_all(other).unwrap();
        for installed in prior {
            assert_eq!(
                crate::os_apps::tenant_os_app_bundle_digest(&tenant, &installed.app_name)
                    .unwrap()
                    .bundle_digest,
                installed.bundle_digest
            );
        }
    }
}

#[tokio::test]
async fn legacy_cache_migration_excludes_changed_unrecorded_and_unverifiable_apps() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let ps = Arc::new(store());
    let platform = server_on(data_dir.path(), &ps);
    let tenant = format!("{TENANT}-legacy-filter");
    let v1 = "1".repeat(40);
    let old = old_cache(&platform, "notes", &v1);
    for app in ["notes", "changed", "unknown", "no-digest"] {
        write_app(&old, app, "original");
    }
    let notes = record(&old, &tenant, "notes", &v1);
    let changed = record(&old, &tenant, "changed", &v1);
    let no_digest = InstalledAppRecord {
        bundle_digest: String::new(),
        ..record(&old, &tenant, "no-digest", &v1)
    };
    write_app(&old, "changed", "tampered");
    let prior = [notes.clone(), changed, no_digest];
    // The root can recover, but neither dependency may be served from the shared old cache.
    assert!(
        ensure_prior_bundle_available(&platform, ps.as_ref(), &notes, &prior)
            .await
            .is_err()
    );
    let destination = cache(&platform, &tenant, "notes", &v1);
    assert_eq!(
        os_app_dir_bundle_digest(&destination, "notes")
            .unwrap()
            .bundle_digest,
        notes.bundle_digest
    );
    for excluded in ["changed", "unknown", "no-digest"] {
        assert!(!destination.join(excluded).exists(), "copied {excluded}");
        assert!(
            crate::os_apps::tenant_app_guide(&tenant, excluded).is_none(),
            "registered {excluded}"
        );
        assert!(
            old.join(excluded).exists(),
            "old cache must remain available"
        );
    }
}

#[tokio::test]
async fn legacy_cache_rejects_a_changed_root_or_one_without_a_recorded_digest() {
    for missing_digest in [false, true] {
        let data_dir = tempfile::tempdir().expect("data dir");
        let ps = Arc::new(store());
        let platform = server_on(data_dir.path(), &ps);
        let tenant = format!("{TENANT}-legacy-reject-{missing_digest}");
        let v1 = "1".repeat(40);
        let old = old_cache(&platform, "notes", &v1);
        write_app(&old, "notes", "original");
        let mut notes = record(&old, &tenant, "notes", &v1);
        if missing_digest {
            notes.bundle_digest.clear();
        } else {
            write_app(&old, "notes", "tampered");
        }
        assert!(
            ensure_prior_bundle_available(
                &platform,
                ps.as_ref(),
                &notes,
                std::slice::from_ref(&notes)
            )
            .await
            .is_err()
        );
        assert!(
            !cache(&platform, &tenant, "notes", &v1)
                .join("notes")
                .exists()
        );
        assert!(crate::os_apps::tenant_app_guide(&tenant, "notes").is_none());
        assert!(old.exists());
    }
}
