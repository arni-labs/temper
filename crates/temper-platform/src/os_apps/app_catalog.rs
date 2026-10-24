use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock, RwLockReadGuard};

use temper_spec::automaton;

use super::{
    AppBundle, AppEntry, StartupInstallMode, extract_description, find_ioa_files, load_app_bundle,
    read_app_guide, read_app_manifest,
};

pub(crate) struct AppCatalog {
    /// Directory containing app bundles.
    apps_dir: PathBuf,
    /// Additional app directories merged into the catalog, with the apps
    /// taken from each.
    additional_dirs: Vec<(PathBuf, DirApps)>,
    /// Catalog entries (lightweight metadata).
    entries: Vec<AppEntry>,
    /// Mapping from app name to its directory path on disk.
    paths: BTreeMap<String, PathBuf>,
    /// Apps installed from Genesis into one tenant other than the platform
    /// tenant, by tenant id.
    ///
    /// Such a tenant resolves its own installs first and the shared catalog
    /// above after them, so its installs change only that tenant: every other
    /// tenant, and every app in the shared catalog, resolves as before.
    tenant_installs: BTreeMap<String, TenantInstalls>,
}

/// Which of a directory's apps the catalog takes from it.
///
/// An install directory holds an app's whole closure. When a dependency in it
/// was installed again separately, the directory still serves the apps that
/// match their records, and the dependency resolves from its own install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirApps {
    /// Every app in the directory.
    Every,
    /// Only the named apps; the directory's other apps are left out.
    Only(BTreeSet<String>),
}

impl DirApps {
    /// The apps either selection takes.
    fn union(self, other: DirApps) -> DirApps {
        match (self, other) {
            (DirApps::Only(mut a), DirApps::Only(b)) => {
                a.extend(b);
                DirApps::Only(a)
            }
            _ => DirApps::Every,
        }
    }
}

/// The apps one tenant installed; a later install of a name replaces an earlier one.
#[derive(Default)]
struct TenantInstalls {
    /// Install directories in the order they were added, with the apps taken
    /// from each.
    dirs: Vec<(PathBuf, DirApps)>,
    entries: BTreeMap<String, AppEntry>,
    paths: BTreeMap<String, PathBuf>,
}

impl TenantInstalls {
    /// Add `installed`, taken from its directory as `apps` says. A directory
    /// added again moves last and takes what it took before as well.
    fn add(&mut self, installed: AppCatalog, apps: DirApps) {
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

/// Global catalog, initialized on first access.
static CATALOG: OnceLock<RwLock<AppCatalog>> = OnceLock::new();

/// Get or initialize the global app catalog.
pub fn catalog() -> &'static RwLock<AppCatalog> {
    CATALOG.get_or_init(|| RwLock::new(AppCatalog::discover()))
}

fn read_catalog() -> RwLockReadGuard<'static, AppCatalog> {
    catalog().read().unwrap() // ci-ok: infallible lock
}

/// List the apps of the shared catalog.
pub fn list_os_apps() -> Vec<AppEntry> {
    read_catalog().app_entries(None)
}

/// List the apps `tenant` resolves: its own installs and the shared catalog.
pub fn list_tenant_os_apps(tenant: &str) -> Vec<AppEntry> {
    read_catalog().app_entries(Some(tenant))
}

/// Get the full spec bundle for a shared-catalog app by name.
///
/// Reads IOA, CSDL, and Cedar files from disk on each call so changes
/// are picked up without a rebuild.
pub fn get_os_app(name: &str) -> Option<AppBundle> {
    load_app_bundle(read_catalog().app_dir(None, name)?)
}

/// Get the full spec bundle `tenant` resolves for `name`: its own install
/// when it has one, otherwise the shared catalog's.
pub fn tenant_os_app(tenant: &str, name: &str) -> Option<AppBundle> {
    load_app_bundle(read_catalog().app_dir(Some(tenant), name)?)
}

/// Get the full app guide markdown `tenant` resolves for `name`.
pub fn tenant_app_guide(tenant: &str, name: &str) -> Option<String> {
    read_catalog()
        .app_entry(Some(tenant), name)
        .and_then(|entry| entry.app_guide.clone())
}

/// Override the OS apps directory. Must be called before any catalog access.
///
/// If the catalog was already initialized, it is replaced.
pub fn set_os_apps_dir(dir: PathBuf) {
    let new_catalog = AppCatalog::from_dir(dir);
    match CATALOG.get() {
        Some(lock) => {
            *lock.write().unwrap() = new_catalog; // ci-ok: infallible lock
        }
        None => {
            let _ = CATALOG.set(RwLock::new(new_catalog));
        }
    }
}

/// Add an additional directory of apps to the catalog.
///
/// Scans the directory and merges discovered apps into the existing catalog.
/// Apps in the new directory do NOT replace existing apps with the same name.
/// Use this to register reference apps or project-specific apps alongside
/// the main os-apps directory.
pub fn add_os_apps_dir(dir: PathBuf) {
    let additional = AppCatalog::from_dir(dir);
    let cat = catalog();
    let mut lock = cat.write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog(additional, DirApps::Every);
}

/// Add `apps` of an app directory and prefer them over existing catalog entries.
///
/// Every tenant resolves these apps. Genesis installs into the platform
/// tenant use this path because a pinned registry closure is the source of
/// truth for the requested install: development/local app catalogs must not
/// shadow the pinned Genesis bundle when both expose the same app name.
pub fn add_os_apps_dir_preferred(dir: PathBuf, apps: DirApps) {
    let additional = AppCatalog::from_dir_taking(dir, &apps);
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.merge_catalog_preferred(additional, apps);
}

/// Make `apps` of `dir` resolve for `tenant` only, ahead of the shared catalog.
///
/// Genesis installs into a tenant other than the platform tenant use this: a
/// pinned closure is what that tenant asked to run, so for that tenant it wins
/// over a shared app of the same name, and no other tenant sees it.
pub fn add_tenant_os_apps_dir(tenant: &str, dir: PathBuf, apps: DirApps) {
    let installed = AppCatalog::from_dir_taking(dir, &apps);
    let mut lock = catalog().write().unwrap(); // ci-ok: infallible lock
    lock.tenant_installs
        .entry(tenant.to_string())
        .or_default()
        .add(installed, apps);
}

/// Re-scan the OS apps directory and refresh the catalog.
///
/// Call this after modifying app files on disk to pick up changes
/// without restarting the server.
pub fn reload_os_apps() {
    let cat = catalog().read().unwrap(); // ci-ok: infallible lock
    let dir = cat.apps_dir.clone();
    let additional_dirs: Vec<(PathBuf, DirApps)> = cat
        .additional_dirs
        .iter()
        .filter(|(dir, _)| dir.is_dir())
        .cloned()
        .collect();
    let tenant_dirs: Vec<(String, Vec<(PathBuf, DirApps)>)> = cat
        .tenant_installs
        .iter()
        .map(|(tenant, installs)| {
            let dirs = installs
                .dirs
                .iter()
                .filter(|(dir, _)| dir.is_dir())
                .cloned();
            (tenant.clone(), dirs.collect())
        })
        .collect();
    drop(cat);
    let mut new = AppCatalog::from_dir(dir);
    for (additional_dir, apps) in additional_dirs {
        let additional = AppCatalog::from_dir_taking(additional_dir, &apps);
        new.merge_catalog(additional, apps);
    }
    for (tenant, dirs) in tenant_dirs {
        let installs = new.tenant_installs.entry(tenant).or_default();
        for (dir, apps) in dirs {
            installs.add(AppCatalog::from_dir_taking(dir, &apps), apps);
        }
    }
    *catalog().write().unwrap() = new; // ci-ok: infallible lock
}

/// List OS apps that belong to the default startup surface.
pub fn list_startup_os_apps() -> Vec<String> {
    let cat = match catalog().read() {
        Ok(cat) => cat,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut apps: Vec<String> = cat
        .entries
        .iter()
        .filter(|entry| entry.startup_install == StartupInstallMode::Core)
        .map(|entry| entry.name.clone())
        .collect();
    apps.sort();
    apps
}

impl AppCatalog {
    /// The directory `name` resolves to for `tenant`: that tenant's own install
    /// when it has one, otherwise the shared catalog's. `None` looks at the
    /// shared catalog only.
    pub(super) fn app_dir(&self, tenant: Option<&str>, name: &str) -> Option<&PathBuf> {
        self.installs(tenant)
            .and_then(|installs| installs.paths.get(name))
            .or_else(|| self.paths.get(name))
    }

    /// The catalog entry `name` resolves to for `tenant`, as [`Self::app_dir`].
    pub(super) fn app_entry(&self, tenant: Option<&str>, name: &str) -> Option<&AppEntry> {
        self.installs(tenant)
            .and_then(|installs| installs.entries.get(name))
            .or_else(|| self.entries.iter().find(|entry| entry.name == name))
    }

    /// Every app `tenant` resolves: its own installs, then the shared apps it
    /// has not replaced.
    pub(super) fn app_entries(&self, tenant: Option<&str>) -> Vec<AppEntry> {
        let installs = self.installs(tenant);
        let mut entries: Vec<AppEntry> = installs
            .map(|installs| installs.entries.values().cloned().collect())
            .unwrap_or_default();
        entries.extend(
            self.entries
                .iter()
                .filter(|entry| installs.is_none_or(|i| !i.entries.contains_key(&entry.name)))
                .cloned(),
        );
        entries
    }

    fn installs(&self, tenant: Option<&str>) -> Option<&TenantInstalls> {
        tenant.and_then(|tenant| self.tenant_installs.get(tenant))
    }

    /// Discover the apps directory and scan it.
    fn discover() -> Self {
        // Priority 1: TEMPER_OS_APPS_DIR env var.
        if let Ok(dir) = std::env::var("TEMPER_OS_APPS_DIR") {
            // determinism-ok: env var read at startup for configuration
            let path = PathBuf::from(dir);
            if path.is_dir() {
                tracing::info!(
                    "Loading OS apps from TEMPER_OS_APPS_DIR: {}",
                    path.display()
                );
                return Self::from_dir(path);
            }
        }

        // Priority 2: Relative to this crate's source (works in dev and cargo test).
        let compile_time_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("os-apps");
        if compile_time_dir.is_dir() {
            let canonical = compile_time_dir
                .canonicalize()
                .unwrap_or(compile_time_dir.clone());
            tracing::info!("Loading OS apps from workspace: {}", canonical.display());
            return Self::from_dir(canonical);
        }

        // Priority 3: ./os-apps/ relative to CWD.
        let cwd_dir = PathBuf::from("os-apps");
        if cwd_dir.is_dir() {
            let canonical = cwd_dir.canonicalize().unwrap_or(cwd_dir.clone());
            tracing::info!("Loading OS apps from CWD: {}", canonical.display());
            return Self::from_dir(canonical);
        }

        tracing::warn!("No os-apps directory found. Set TEMPER_OS_APPS_DIR for dev/local apps.");
        Self {
            apps_dir: PathBuf::new(),
            additional_dirs: Vec::new(),
            entries: Vec::new(),
            paths: BTreeMap::new(),
            tenant_installs: BTreeMap::new(),
        }
    }

    pub(super) fn from_dir(dir: PathBuf) -> Self {
        let mut entries = Vec::new();
        let mut paths = BTreeMap::new();

        let read_dir = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!("Failed to read apps directory {}: {e}", dir.display());
                return Self {
                    apps_dir: dir,
                    additional_dirs: Vec::new(),
                    entries,
                    paths,
                    tenant_installs: BTreeMap::new(),
                };
            }
        };

        let mut app_dirs: Vec<_> = read_dir
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        app_dirs.sort_by_key(|e| e.file_name());

        for entry in app_dirs {
            let app_dir = entry.path();
            let dir_name = entry.file_name().to_string_lossy().to_string();

            let manifest = match read_app_manifest(&app_dir) {
                Some(m) => m,
                None => {
                    tracing::warn!(
                        app = %dir_name,
                        path = %app_dir.display(),
                        "Skipping app directory — missing required app.toml"
                    );
                    continue;
                }
            };

            let app_guide = match read_app_guide(&app_dir) {
                Some(guide) => Some(guide),
                None => {
                    tracing::warn!(
                        app = %manifest.name,
                        path = %app_dir.display(),
                        "Skipping app — missing required APP.md"
                    );
                    continue;
                }
            };

            let app_name = manifest.name.clone();
            let ioa_files = find_ioa_files(&app_dir);
            let entity_types: Vec<String> = ioa_files
                .iter()
                .filter_map(|(_, ioa_path)| {
                    let source = std::fs::read_to_string(ioa_path).ok()?;
                    let parsed = automaton::parse_automaton(&source).ok()?;
                    Some(parsed.automaton.name)
                })
                .collect();

            let description = if !manifest.description.is_empty() {
                manifest.description.clone()
            } else {
                app_guide
                    .as_ref()
                    .and_then(|guide| extract_description(guide))
                    .unwrap_or_else(|| format!("App: {app_name}"))
            };

            let version = manifest.version.clone();
            let startup_install = manifest.startup_install;
            let dependencies = manifest.dependencies.clone();

            let app_path = app_dir.clone();
            paths.insert(app_name.clone(), app_path.clone());
            if dir_name != app_name {
                paths.entry(dir_name).or_insert(app_path);
            }
            entries.push(AppEntry {
                name: app_name,
                description,
                entity_types,
                version,
                startup_install,
                app_guide,
                dependencies,
            });
        }

        Self {
            apps_dir: dir,
            additional_dirs: Vec::new(),
            entries,
            paths,
            tenant_installs: BTreeMap::new(),
        }
    }

    /// The apps of `dir` that `apps` takes, as [`Self::from_dir`] reads them.
    fn from_dir_taking(dir: PathBuf, apps: &DirApps) -> Self {
        let mut catalog = Self::from_dir(dir);
        if let DirApps::Only(names) = apps {
            catalog.entries.retain(|entry| names.contains(&entry.name));
            let kept: BTreeSet<PathBuf> = catalog
                .entries
                .iter()
                .filter_map(|entry| catalog.paths.get(&entry.name).cloned())
                .collect();
            catalog.paths.retain(|_, path| kept.contains(path));
        }
        catalog
    }

    /// Remember `dir` and the apps taken from it, so a reload takes the same.
    fn remember_dir(&mut self, dir: &PathBuf, apps: DirApps) {
        if !dir.is_dir() {
            return;
        }
        match self
            .additional_dirs
            .iter_mut()
            .find(|(known, _)| known == dir)
        {
            Some((_, taken)) => *taken = std::mem::replace(taken, DirApps::Every).union(apps),
            None => self.additional_dirs.push((dir.clone(), apps)),
        }
    }

    fn merge_catalog(&mut self, additional: AppCatalog, apps: DirApps) {
        self.remember_dir(&additional.apps_dir, apps);
        for (name, path) in additional.paths {
            self.paths.entry(name).or_insert(path);
        }
        for entry in additional.entries {
            if !self
                .entries
                .iter()
                .any(|existing| existing.name == entry.name)
            {
                self.entries.push(entry);
            }
        }
    }

    fn merge_catalog_preferred(&mut self, additional: AppCatalog, apps: DirApps) {
        self.remember_dir(&additional.apps_dir, apps);
        for (name, path) in additional.paths {
            self.paths.insert(name, path);
        }
        for entry in additional.entries {
            if let Some(existing) = self
                .entries
                .iter_mut()
                .find(|existing| existing.name == entry.name)
            {
                *existing = entry;
            } else {
                self.entries.push(entry);
            }
        }
    }
}
