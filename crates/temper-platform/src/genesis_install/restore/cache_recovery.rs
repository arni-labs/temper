//! Find the cache directory that still holds an install's recorded versions.

use super::super::written;
use super::*;
use crate::os_apps::written_apps_bundle_digests;

/// The cache directories an install of `cache_key` into `tenant` may have, the registry's first.
///
/// A registry install caches under its registry tenant. An `App.Install` from a Genesis service
/// on this server caches under the service's tenant, which its record names as the registry
/// tenant unless the request named another one, so every Genesis source tenant is a candidate too.
/// A pushed install caches under its tenant alone, after every Genesis source.
/// A candidate is used only when it holds the recorded versions.
pub(super) fn install_cache_candidates(
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
    sources.push(InstallSource::Pushed);
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

/// The first of `candidates` that holds `record`'s app at its recorded version,
/// with the apps to take from it.
pub(super) async fn first_matching_install(
    ps: &dyn PlatformStore,
    candidates: &[PathBuf],
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<(PathBuf, Vec<(String, AppFiles)>)> {
    for candidate in candidates {
        if let Some(apps) = cached_apps_matching_records(ps, candidate, record, recorded).await {
            return Some((candidate.clone(), apps));
        }
    }
    None
}

/// The apps of an install's cache directory that still match their records,
/// each with the files its install wrote, or `None` when `record`'s own app
/// is not there at its recorded version.
///
/// A directory on disk is not proof on its own: an install could have stopped
/// partway, and a file in it could have changed since. Its apps are the ones
/// its install's list names ([`written`]), read by exact path, never by
/// listing the directory: a directory with no list, or whose list cannot be
/// read, is not used, and the caller fetches the files again. Each app in it
/// is decided on its own: it is taken when it reads back at its recorded
/// digest, from `recorded` when that names the app, otherwise from the
/// tenant's stored record. A record without a digest needs the app to read
/// back. An app with no record, or at another version than its record names (a
/// dependency installed again separately), is left out, so the tenant goes on
/// resolving it from wherever it resolves it now. The apps are decided before
/// they are registered, and are registered from the same list, so an app that
/// does not match never becomes what the tenant resolves.
pub(super) async fn cached_apps_matching_records(
    ps: &dyn PlatformStore,
    cache_root: &Path,
    record: &InstalledAppRecord,
    recorded: &[InstalledAppRecord],
) -> Option<Vec<(String, AppFiles)>> {
    if !cache_root.is_dir() {
        return None;
    }
    let apps = match written::read(cache_root) {
        Ok(apps) => apps,
        Err(error) => {
            tracing::warn!(
                tenant = %record.tenant,
                app = %record.app_name,
                error = %error,
                "An install's cache directory has no file list it can be read from; it is not used"
            );
            return None;
        }
    };
    let digests = written_apps_bundle_digests(cache_root, apps.clone());
    let mut matching = BTreeSet::new();
    for digest in &digests {
        let expected = match recorded.iter().find(|r| r.app_name == digest.app_name) {
            Some(known) => known.bundle_digest.clone(),
            None => match ps.get_installed_app(&record.tenant, &digest.app_name).await {
                Ok(Some(stored)) => stored.bundle_digest,
                _ => continue,
            },
        };
        if expected.is_empty() || expected == digest.bundle_digest {
            matching.insert(digest.app_name.clone());
        }
    }
    if !matching.contains(&record.app_name) {
        tracing::warn!(
            tenant = %record.tenant,
            app = %record.app_name,
            cache = %cache_root.display(),
            "An install's cache directory does not hold its recorded version; it is not used"
        );
        return None;
    }
    Some(
        apps.into_iter()
            .filter(|(app, _)| matching.contains(app))
            .collect(),
    )
}
