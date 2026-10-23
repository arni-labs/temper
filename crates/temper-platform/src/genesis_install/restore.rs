//! Bringing installs this server already made back: at boot, and for rollback.
//!
//! Restoring reproduces a stored install. It reuses the install's own cache
//! directory when that still holds the recorded version, and otherwise fetches
//! the same version again from where the install came from. Which registry
//! new installs come from is decided at install time, not here.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::InstalledAppRecord;

use super::registry::recorded_registry;
use super::*;
use crate::os_apps::os_app_dir_bundle_digest;

/// Rebuild Genesis app materialization cache roots from durable Genesis rows.
///
/// This runs during server boot before persisted app installs are replayed. It
/// keeps recovery spec-first: `AppInstallation` rows point at pinned Genesis
/// `App`/`Commit`/`Tree`/`Blob` state, and the local OS-app catalog is rebuilt
/// from those objects instead of from GitHub, submodules, or arbitrary app dirs.
pub async fn restore_genesis_app_cache_roots(platform: &PlatformState) -> usize {
    let source_tenants = genesis_source_tenants();
    let mut restored = 0usize;

    for source_tenant in source_tenants {
        let tenant = TenantId::new(&source_tenant);
        let installation_ids = platform
            .server
            .list_entity_ids_lazy(&tenant, "AppInstallation")
            .await;
        for installation_id in installation_ids {
            let Ok(installation) = platform
                .server
                .get_tenant_entity_state(&tenant, "AppInstallation", &installation_id)
                .await
            else {
                continue;
            };
            if installation.state.status != "Installed" {
                continue;
            }
            let Some(app_id) = string_field(&installation.state.fields, "AppId") else {
                continue;
            };
            let Ok(app) = platform
                .server
                .get_tenant_entity_state(&tenant, "App", &app_id)
                .await
            else {
                continue;
            };

            let fields = &app.state.fields;
            let Some(name) = string_field(fields, "Name") else {
                continue;
            };
            let Some(owner) = string_field(fields, "OwnerId") else {
                continue;
            };
            let Some(repository_id) = string_field(fields, "RepositoryId") else {
                continue;
            };
            let version_hash = string_field(&installation.state.fields, "VersionHash")
                .or_else(|| string_field(fields, "LatestVersionHash"));
            let Some(version_hash) = version_hash else {
                continue;
            };
            let app_ref = string_field(&installation.state.fields, "AppRef").unwrap_or_else(|| {
                format!("{owner}/{name}@{}", version_hash.trim_start_matches('@'))
            });
            let target_tenant = string_field(&installation.state.fields, "TargetTenant")
                .filter(|target| !target.trim().is_empty())
                .unwrap_or_else(|| source_tenant.clone());
            let cache_root = genesis_cache_root(
                &platform.server,
                CacheOwner::Install {
                    tenant: &target_tenant,
                    source: InstallSource::Local(&source_tenant),
                },
                &app_ref,
            );
            let root = GenesisAppBundle {
                owner,
                name,
                repository_id,
                version_hash,
            };
            match materialize_app_closure(&platform.server, &tenant, &cache_root, root).await {
                Ok(_) => {
                    legacy_specs::convert_legacy_bundle_specs(&cache_root);
                    register_install_cache(&target_tenant, cache_root);
                    restored += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        source_tenant = %source_tenant,
                        app_id = %app_id,
                        app_ref = %app_ref,
                        error = %error,
                        "Failed to restore Genesis app cache root"
                    );
                }
            }
        }
    }

    restored
}

/// Rebuild local cache roots for apps that were installed from a remote Genesis
/// registry into this Temper instance.
///
/// These rows live in the target instance's durable installed-app table. They
/// are distinct from Genesis service-side `AppInstallation` rows above. On
/// restart, recovering the pinned cache roots first lets the normal runtime
/// recovery/reconcile path validate digests without re-dispatching
/// spec-owned `App.Install` or rerunning seed data for unchanged refs.
///
/// Every stored install comes back, whatever the registry setting is now: from
/// its cache directory when that holds the recorded version, otherwise fetched
/// again from the registry it was installed from. An install whose files can
/// be neither found nor fetched keeps its record, and the reason is logged.
pub async fn restore_genesis_registry_cache_roots(platform: &PlatformState) -> usize {
    let Some(ps) = platform
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return 0;
    };

    let installed = match ps.list_all_installed_apps().await {
        Ok(installed) => installed,
        Err(error) => {
            tracing::warn!(error = %error, "Failed to list installed apps for Genesis cache recovery");
            return 0;
        }
    };

    let mut restored = 0usize;
    let mut seen = BTreeSet::new();
    for (tenant, app_name) in installed {
        let record = match ps.get_installed_app(&tenant, &app_name).await {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    error = %error,
                    "Failed to read installed app metadata for Genesis cache recovery"
                );
                continue;
            }
        };
        if record.source_kind != "genesis" || record.registry_url.trim().is_empty() {
            continue;
        }
        let root_ref = match parse_registry_app_ref(&record.app_ref) {
            Ok(root_ref) => root_ref,
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Installed app has invalid Genesis app_ref"
                );
                continue;
            }
        };
        let registry_tenant = if record.registry_tenant.trim().is_empty() {
            "default"
        } else {
            record.registry_tenant.trim()
        };
        let cache_root = genesis_cache_root(
            &platform.server,
            CacheOwner::Install {
                tenant: &tenant,
                source: InstallSource::Registry(registry_tenant),
            },
            &record.app_ref,
        );
        // Each tenant's install has its own directory, so records restore it
        // once per directory, from that install's own registry tenant.
        if !seen.insert(cache_root.clone()) {
            continue;
        }
        if stored_install_matches(&cache_root, &record) {
            register_install_cache(&tenant, cache_root);
            restored += 1;
            continue;
        }
        let registry = match recorded_registry(&record.registry_url) {
            Ok(registry) => registry,
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Installed Genesis app's files are not on disk and its recorded registry cannot be fetched from; its install record is kept"
                );
                continue;
            }
        };
        let materialized = match materialize_registry_app_closure_via_bundle(
            &registry,
            registry_tenant,
            root_ref.clone(),
            &cache_root,
        )
        .await
        {
            Ok(materialized) => Ok(materialized),
            Err(error) if genesis_git_fallback_enabled() => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    registry_url = %registry.as_str(),
                    error = %error,
                    "Genesis bundle cache recovery failed; falling back to git clone because TEMPER_GENESIS_INSTALL_GIT_FALLBACK is enabled"
                );
                materialize_registry_app_closure(&registry, root_ref, &cache_root).await
            }
            Err(error) => Err(error),
        };
        match materialized {
            Ok(_) => {
                legacy_specs::convert_legacy_bundle_specs(&cache_root);
                register_install_cache(&tenant, cache_root);
                restored += 1;
            }
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    app = %app_name,
                    app_ref = %record.app_ref,
                    error = %error,
                    "Failed to fetch installed Genesis app files again; its install record is kept"
                );
            }
        }
    }

    restored
}

/// Make sure the prior version's bundle is resolvable from the app catalog before rollback.
///
/// Prefers the prior ref's on-disk cache; if it was evicted, re-materializes the closure from the
/// prior record's registry coordinates. Best-effort when the prior ref is unpinned or carries no
/// registry URL — rollback then relies on whatever bundle the catalog already resolves for the app.
pub(super) async fn ensure_prior_bundle_available(
    platform: &PlatformState,
    prior: &InstalledAppRecord,
) -> Result<(), String> {
    let prior_ref = parse_registry_app_ref(&prior.app_ref)?;
    let Some(prior_hash) = prior_ref.version_hash.clone() else {
        return Ok(());
    };
    let cache_key = format!(
        "{}/{}@{}",
        prior_ref.owner,
        prior_ref.name,
        prior_hash.trim_start_matches('@')
    );
    let registry_tenant = if prior.registry_tenant.trim().is_empty() {
        "default".to_string()
    } else {
        prior.registry_tenant.trim().to_string()
    };
    let cache_root = genesis_cache_root(
        &platform.server,
        CacheOwner::Install {
            tenant: &prior.tenant,
            source: InstallSource::Registry(&registry_tenant),
        },
        &cache_key,
    );
    if stored_install_matches(&cache_root, prior) {
        register_install_cache(&prior.tenant, cache_root);
        return Ok(());
    }
    if cache_root.is_dir() {
        tracing::warn!(
            app = %prior.app_name,
            app_ref = %prior.app_ref,
            "Prior Genesis cache did not yield the expected bundle; re-materializing for rollback"
        );
    }
    if prior.registry_url.trim().is_empty() {
        return Ok(());
    }
    let registry = recorded_registry(&prior.registry_url)?;
    std::fs::create_dir_all(&cache_root).map_err(|error| {
        format!(
            "create Genesis rollback cache '{}': {error}",
            cache_root.display()
        )
    })?;
    materialize_registry_app_closure_via_bundle(
        &registry,
        &registry_tenant,
        prior_ref,
        &cache_root,
    )
    .await
    .map_err(|error| {
        format!(
            "failed to re-materialize previous version {} for rollback: {error}",
            prior.app_ref
        )
    })?;
    register_install_cache(&prior.tenant, cache_root);
    Ok(())
}

/// Whether an install's cache directory still holds the version its record names.
///
/// A directory on disk is not proof on its own: an install could have stopped
/// partway. It counts only when the recorded app reads back from it at the
/// recorded digest, or, for a record without a digest, at least reads back.
/// It is checked before it is registered, so a directory that does not match
/// never becomes what the tenant resolves.
fn stored_install_matches(cache_root: &Path, record: &InstalledAppRecord) -> bool {
    match os_app_dir_bundle_digest(cache_root, &record.app_name) {
        Some(digest) => {
            record.bundle_digest.is_empty() || digest.bundle_digest == record.bundle_digest
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_app(root: &Path, app_name: &str, marker: &str) {
        let app_dir = root.join(app_name);
        std::fs::create_dir_all(&app_dir).expect("app dir");
        std::fs::write(app_dir.join("app.toml"), format!("name = \"{app_name}\"\n"))
            .expect("app.toml");
        std::fs::write(
            app_dir.join("APP.md"),
            format!("# {app_name}\n\n{marker}\n"),
        )
        .expect("APP.md");
    }

    #[test]
    fn a_cache_directory_is_reused_only_while_it_holds_the_recorded_version() {
        let root = tempfile::tempdir().expect("cache root");
        write_app(root.path(), "notes", "first version");
        let recorded = os_app_dir_bundle_digest(root.path(), "notes").expect("digest");
        let record = InstalledAppRecord {
            app_name: "notes".to_string(),
            bundle_digest: recorded.bundle_digest,
            ..Default::default()
        };
        assert!(stored_install_matches(root.path(), &record));

        write_app(root.path(), "notes", "another version");
        assert!(!stored_install_matches(root.path(), &record));
        assert!(!stored_install_matches(&root.path().join("gone"), &record));
    }
}
