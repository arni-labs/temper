//! Adoption: an app this server installs at boot from its boot catalog, with
//! no pushed version yet, becomes its own first pushed version.
//!
//! Why: an app that was running before pushes existed has only the catalog
//! version it was started with. Once its files are pushed, the pushed version
//! is what boots; until then, a boot whose catalog changed or went away would
//! serve something else, or nothing. Adopting the version it serves keeps the
//! app on its last served version from then on, whatever its catalog holds
//! later. Adoption runs no checks and refuses nothing: the version is already
//! serving, and a version whose files fail today's checks must keep serving
//! until its owner pushes a new one, not stop at a restart.
//!
//! This is a migration path for apps that predate pushes. Delete it, and its
//! call at boot in temper-cli `serve/bootstrap.rs`, once every production app
//! has a pushed root (tracked on the Katagami hub as "Remove adoption once
//! every prod app has a pushed root"). Katagami's start script stops pointing
//! the boot catalog at an app's old checkout at the same point.
//!
//! The whole app directory is copied, so a `COMMIT` file the catalog's copy
//! holds (Katagami writes `<app>/COMMIT` into the checkout it adopts) names the
//! adopted version's commit, as it does for a pushed one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::PoisonError;

use super::files;
use crate::os_apps::{DirApps, add_tenant_os_apps_dir, catalog};
use crate::state::PlatformState;

/// Make the version of `app` that `tenant` resolves now its first pushed
/// version, when it has none. Returns the adopted digest, or `None` when the
/// app already has a pushed version or no files to adopt.
///
/// Every file under the app's directory is copied, as the installer may read
/// any of them; the copy is written beside its place and renamed into it, and
/// `current` is written atomically, so a crash leaves either no pushed version
/// (the next boot adopts again) or a whole one.
pub fn adopt_catalog_app(
    state: &PlatformState,
    tenant: &str,
    app: &str,
) -> Result<Option<String>, String> {
    let home = files::app_home(&state.server, tenant, app)?;
    if files::current(&home).is_some() {
        return Ok(None);
    }
    let resolved = catalog()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .app_dir(Some(tenant), app)
        .cloned();
    let Some(app_dir) = resolved else {
        return Ok(None);
    };
    let tree = read_tree(&app_dir).map_err(|error| {
        format!(
            "the files of '{app}' at {} could not be read: {error}",
            app_dir.display()
        )
    })?;
    if tree.is_empty() {
        return Ok(None);
    }
    let digest = files::digest(&tree);
    let root = home.join(&digest);
    if !root.is_dir() {
        files::materialize(&home, &digest, app, &tree)
            .map_err(|error| format!("the files of '{app}' could not be copied: {error}"))?;
    }
    files::set_current(&home, Some(&digest)).map_err(|error| {
        format!("the adopted version of '{app}' could not be recorded: {error}")
    })?;
    add_tenant_os_apps_dir(
        tenant,
        root,
        DirApps::Only(BTreeSet::from([app.to_string()])),
    );
    Ok(Some(digest))
}

/// Every regular file under `dir`, by its `/`-separated path relative to it.
fn read_tree(dir: &Path) -> std::io::Result<BTreeMap<String, Vec<u8>>> {
    let mut tree = BTreeMap::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(next) = dirs.pop() {
        for entry in std::fs::read_dir(&next)? {
            let path = entry?.path();
            let metadata = std::fs::metadata(&path)?;
            if metadata.is_dir() {
                dirs.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = path.strip_prefix(dir).map_err(std::io::Error::other)?;
            let parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            tree.insert(parts.join("/"), std::fs::read(&path)?);
        }
    }
    Ok(tree)
}
