//! The apps one tenant installed for itself, which it resolves ahead of the
//! shared catalog.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::PoisonError;

use super::{AppCatalog, AppEntry, DirApps, catalog};

/// The apps one tenant installed; a later install of a name replaces an earlier one.
#[derive(Default)]
pub(super) struct TenantInstalls {
    /// Install directories in the order they were added, with the apps taken
    /// from each.
    pub(super) dirs: Vec<(PathBuf, DirApps)>,
    pub(super) entries: BTreeMap<String, AppEntry>,
    pub(super) paths: BTreeMap<String, PathBuf>,
}

impl TenantInstalls {
    /// Add `installed`, taken from its directory as `apps` says. A directory
    /// added again moves last and takes what it took before as well.
    pub(super) fn add(&mut self, installed: AppCatalog, apps: DirApps) {
        let apps = match self
            .dirs
            .iter()
            .position(|(dir, _)| dir == &installed.apps_dir)
        {
            Some(index) => self.dirs.remove(index).1.union(apps),
            None => apps,
        };
        self.dirs.push((installed.apps_dir, apps));
        self.paths.extend(installed.paths);
        for entry in installed.entries {
            self.entries.insert(entry.name.clone(), entry);
        }
    }
}

/// Make `apps` of `dir` resolve for `tenant` only, ahead of the shared catalog.
///
/// Genesis installs into a tenant other than the platform tenant use this: a
/// pinned closure is what that tenant asked to run, so for that tenant it wins
/// over a shared app of the same name, and no other tenant sees it. An app's
/// files pushed to its own server use it too, in every tenant.
pub fn add_tenant_os_apps_dir(tenant: &str, dir: PathBuf, apps: DirApps) {
    let installed = AppCatalog::from_dir_taking(dir, &apps);
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.tenant_installs
        .entry(tenant.to_string())
        .or_default()
        .add(installed, apps);
}

/// Stop `dir` resolving for `tenant`. Its apps resolve again as the tenant's
/// other install directories, in the order they were added, and then the
/// shared catalog say: this undoes an [`add_tenant_os_apps_dir`] whose install
/// did not go in, and forgets a directory deleted from disk. A directory the
/// tenant does not resolve changes nothing.
pub fn remove_tenant_os_apps_dir(tenant: &str, dir: &Path) {
    let mut lock = catalog().write().unwrap_or_else(PoisonError::into_inner);
    let Some(installs) = lock.tenant_installs.get_mut(tenant) else {
        return;
    };
    let Some(index) = installs.dirs.iter().position(|(known, _)| known == dir) else {
        return;
    };
    installs.dirs.remove(index);
    // The other directories are read again in their order, so each app
    // resolves to the last of them that holds it.
    let mut rebuilt = TenantInstalls::default();
    for (dir, apps) in std::mem::take(&mut installs.dirs) {
        rebuilt.add(AppCatalog::from_dir_taking(dir, &apps), apps);
    }
    if rebuilt.dirs.is_empty() {
        lock.tenant_installs.remove(tenant);
    } else {
        *installs = rebuilt;
    }
}
