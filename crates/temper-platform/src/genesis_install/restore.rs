//! Bringing installs this server already made back: at boot, and for rollback.
//!
//! Restoring reproduces a stored install. It reuses the install's own cache
//! directory when that still holds the recorded version, and otherwise fetches
//! the same version again from where the install came from. Which registry
//! new installs come from is decided at install time, not here.

use std::collections::BTreeSet;

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{InstalledAppRecord, PlatformStore};

use super::registry::recorded_registry;
use super::*;
use crate::os_apps::os_app_dir_bundle_digests;

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
        let candidates =
            install_cache_candidates(platform, &tenant, registry_tenant, &record.app_ref);
        let cache_root = candidates[0].clone();
        // Each tenant's install has its own directory, so records restore it
        // once per directory, from that install's own registry tenant.
        if !seen.insert(cache_root.clone()) {
            continue;
        }
        if let Some(stored) = first_matching_install(
            ps.as_ref(),
            &candidates,
            &record,
            std::slice::from_ref(&record),
        )
        .await
        {
            register_install_cache(&tenant, stored);
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
/// Prefers the prior install's on-disk cache, whether a registry install or an `App.Install`
/// made it; if it was evicted, re-materializes the closure from the prior record's registry
/// coordinates. `prior_records` are the closure's records from before the failed install, which
/// has since overwritten the stored ones. Best-effort when the prior ref is unpinned or carries no
/// registry URL — rollback then relies on whatever bundle the catalog already resolves for the app.
pub(super) async fn ensure_prior_bundle_available(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    prior: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
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
    let candidates =
        install_cache_candidates(platform, &prior.tenant, &registry_tenant, &cache_key);
    if let Some(stored) = first_matching_install(ps, &candidates, prior, prior_records).await {
        register_install_cache(&prior.tenant, stored);
        return Ok(());
    }
    let cache_root = candidates[0].clone();
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

/// The cache directories an install of `cache_key` into `tenant` may have, the registry's first.
///
/// A registry install caches under its registry tenant. An `App.Install` from a Genesis service
/// on this server caches under the service's tenant, which its record names as the registry
/// tenant unless the request named another one, so every Genesis source tenant is a candidate too.
/// A candidate is used only when it holds the recorded versions.
fn install_cache_candidates(
    platform: &PlatformState,
    tenant: &str,
    registry_tenant: &str,
    cache_key: &str,
) -> Vec<PathBuf> {
    let mut sources = vec![
        InstallSource::Registry(registry_tenant),
        InstallSource::Local(registry_tenant),
    ];
    let source_tenants = genesis_source_tenants();
    sources.extend(
        source_tenants
            .iter()
            .filter(|source| source.as_str() != registry_tenant)
            .map(|source| InstallSource::Local(source.as_str())),
    );
    sources
        .into_iter()
        .map(|source| {
            genesis_cache_root(
                &platform.server,
                CacheOwner::Install { tenant, source },
                cache_key,
            )
        })
        .collect()
}

/// The first of `candidates` that still holds the versions `record`'s install recorded.
async fn first_matching_install(
    ps: &dyn PlatformStore,
    candidates: &[PathBuf],
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<PathBuf> {
    for candidate in candidates {
        if stored_install_matches(ps, candidate, record, recorded).await {
            return Some(candidate.clone());
        }
    }
    None
}

/// Whether an install's cache directory still holds the versions its records name.
///
/// A directory on disk is not proof on its own: an install could have stopped
/// partway, and a file in it could have changed since. Registering the
/// directory makes every app in it what the tenant resolves, so every app in
/// it must read back at its recorded digest: from `recorded` when that names
/// the app, otherwise from the tenant's stored record. A record without a
/// digest needs the app to read back. The installed app itself must be in the
/// directory. It is checked before it is registered, so a directory that does
/// not match never becomes what the tenant resolves.
async fn stored_install_matches(
    ps: &dyn PlatformStore,
    cache_root: &Path,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> bool {
    let Some(digests) = os_app_dir_bundle_digests(cache_root) else {
        return false;
    };
    if !digests
        .iter()
        .any(|digest| digest.app_name == record.app_name)
    {
        return false;
    }
    for digest in &digests {
        let expected = match recorded.iter().find(|r| r.app_name == digest.app_name) {
            Some(known) => known.bundle_digest.clone(),
            None => match ps.get_installed_app(&record.tenant, &digest.app_name).await {
                Ok(Some(stored)) => stored.bundle_digest,
                _ => return false,
            },
        };
        if !expected.is_empty() && expected != digest.bundle_digest {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "restore/tests.rs"]
mod tests;
