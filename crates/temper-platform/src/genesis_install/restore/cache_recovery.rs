//! Find current or pre-tenant caches without registering unverified shared files.

use super::super::cache_paths::legacy_genesis_cache_root;
use super::*;

/// A tenant-owned cache or a shared cache that needs verified migration.
pub(super) struct InstallCacheCandidate {
    /// Filesystem location of this recovery candidate.
    pub(super) path: PathBuf,
    legacy: bool,
}

/// The cache directories an install of `cache_key` into `tenant` may have, the registry's first.
///
/// A registry install caches under its registry tenant. An `App.Install` from a Genesis service
/// on this server caches under the service's tenant, which its record names as the registry
/// tenant unless the request named another one, so every Genesis source tenant is a candidate too.
/// A pushed install caches under its tenant alone, after every Genesis source.
/// A candidate is used only when it holds the recorded versions. Old shared caches
/// require recorded digests and are copied into the first tenant-owned candidate.
pub(super) fn install_cache_candidates(
    platform: &PlatformState,
    tenant: &str,
    registry_tenant: &str,
    cache_key: &str,
) -> Vec<InstallCacheCandidate> {
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
    sources.push(InstallSource::Pushed);
    let mut candidates: Vec<_> = sources
        .into_iter()
        .map(|source| InstallCacheCandidate {
            path: genesis_cache_root(
                &platform.server,
                CacheOwner::Install { tenant, source },
                cache_key,
            ),
            legacy: false,
        })
        .collect();
    candidates.push(InstallCacheCandidate {
        path: legacy_genesis_cache_root(&platform.server, cache_key),
        legacy: true,
    });
    candidates
}

/// The first of `candidates` that holds `record`'s app at its recorded version,
/// with the apps to take from it.
pub(super) async fn first_matching_install(
    ps: &dyn PlatformStore,
    candidates: &[InstallCacheCandidate],
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<(PathBuf, BTreeSet<String>)> {
    for candidate in candidates {
        let Some(apps) =
            matching_cached_apps(ps, &candidate.path, record, recorded, candidate.legacy).await
        else {
            continue;
        };
        if !candidate.legacy {
            return Some((candidate.path.clone(), apps));
        }
        // Never register the shared directory: copy only apps proven against this tenant's
        // records, then verify the copy before publishing it to the tenant-owned destination.
        let destination = &candidates[0].path;
        match migrate_legacy_cache(ps, &candidate.path, destination, &apps, record, recorded).await
        {
            Ok(apps) => return Some((destination.clone(), apps)),
            Err(error) => tracing::warn!(
                tenant = %record.tenant,
                app = %record.app_name,
                cache = %candidate.path.display(),
                error = %error,
                "Failed to recover legacy Genesis cache; trying other recovery sources"
            ),
        }
    }
    None
}

/// The apps of an install's cache directory that still match their records,
/// or `None` when `record`'s own app is not there at its recorded version.
///
/// A directory on disk is not proof on its own: an install could have stopped
/// partway, and a file in it could have changed since. Each app in it is
/// decided on its own: it is taken when it reads back at its recorded digest,
/// from `recorded` when that names the app, otherwise from the tenant's stored
/// record. A record without a digest needs the app to read back. An app with
/// no record, or at another version than its record names (a dependency
/// installed again separately), is left out, so the tenant goes on resolving
/// it from wherever it resolves it now. The apps are decided before they are
/// registered, so an app that does not match never becomes what the tenant
/// resolves.
pub(super) async fn cached_apps_matching_records(
    ps: &dyn PlatformStore,
    cache_root: &Path,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<BTreeSet<String>> {
    matching_cached_apps(ps, cache_root, record, recorded, false).await
}

async fn matching_cached_apps(
    ps: &dyn PlatformStore,
    cache_root: &Path,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
    require_digest: bool,
) -> Option<BTreeSet<String>> {
    let digests = os_app_dir_bundle_digests(cache_root)?;
    let mut matching = BTreeSet::new();
    for digest in &digests {
        let expected = match recorded.iter().find(|r| r.app_name == digest.app_name) {
            Some(known) => known.bundle_digest.clone(),
            None => match ps.get_installed_app(&record.tenant, &digest.app_name).await {
                Ok(Some(stored)) => stored.bundle_digest,
                _ => continue,
            },
        };
        if (!require_digest && expected.is_empty()) || expected == digest.bundle_digest {
            matching.insert(digest.app_name.clone());
        }
    }
    matching.contains(&record.app_name).then_some(matching)
}

async fn migrate_legacy_cache(
    ps: &dyn PlatformStore,
    source: &Path,
    destination: &Path,
    apps: &BTreeSet<String>,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Result<BTreeSet<String>, String> {
    let parent = destination.parent().ok_or("Genesis cache has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("create tenant cache parent: {error}"))?;
    let staged = tempfile::Builder::new()
        .prefix(".genesis-migrate-")
        .tempdir_in(parent)
        .map_err(|error| format!("stage legacy Genesis cache: {error}"))?;
    for app in apps {
        let source = app_cache_dir(source, app)?;
        let target = app_cache_dir(staged.path(), app)?;
        copy_app_directory(&source, &target)?;
    }
    let verified = matching_cached_apps(ps, staged.path(), record, recorded, true)
        .await
        .filter(|verified| verified == apps)
        .ok_or("legacy Genesis apps changed while copying")?;
    replace_directory(staged.keep(), destination)?;
    Ok(verified)
}

/// Copy regular files into a fresh staging directory; never follow shared cache symlinks.
fn copy_app_directory(source: &Path, destination: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("stat legacy Genesis cache '{}': {error}", source.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "legacy Genesis cache '{}' is not a directory",
            source.display()
        ));
    }
    std::fs::create_dir(destination)
        .map_err(|error| format!("create migrated Genesis directory: {error}"))?;
    for entry in std::fs::read_dir(source)
        .map_err(|error| format!("read legacy Genesis directory: {error}"))?
    {
        let entry = entry.map_err(|error| format!("read legacy Genesis entry: {error}"))?;
        let source = entry.path();
        let target = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|error| format!("stat legacy Genesis entry: {error}"))?;
        if kind.is_dir() {
            copy_app_directory(&source, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target).map_err(|error| {
                format!("copy legacy Genesis file '{}': {error}", source.display())
            })?;
        } else {
            return Err(format!(
                "legacy Genesis entry '{}' is not a regular file or directory",
                source.display()
            ));
        }
    }
    Ok(())
}
