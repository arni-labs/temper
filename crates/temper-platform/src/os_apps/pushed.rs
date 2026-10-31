//! An app's files pushed to its own running server, checked and installed in
//! place (`POST /api/os-apps/{name}/install`).
//!
//! The caller sends every file of the app. The server writes them into a
//! directory named by their digest, checks each spec through the same
//! budgeted cascade as `POST /api/specs/validate-ioa`, makes that directory
//! the tenant's install of the app ahead of any other, and runs the installer
//! every install runs ([`crate::install_os_app`]). A push refused at any step
//! leaves the catalog, registry and records as they were. A restart installs
//! the version installed last again ([`restore_pushed_app_roots`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

use serde::Deserialize;
use temper_verify::CascadeResult;

use super::{
    DirApps, InstallResult, add_tenant_os_apps_dir, catalog, find_ioa_files, read_app_guide,
    read_app_manifest, remove_tenant_os_apps_dir,
};
use crate::state::PlatformState;

mod adopt;
mod files;

pub use adopt::adopt_catalog_app;

/// One pushed file: its text, its bytes in base64, or the same bytes as the
/// file at its path in the version installed now, named by their SHA-256.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum PushedFile {
    /// UTF-8 text.
    Text(String),
    /// Bytes, base64-encoded.
    Bytes(Base64File),
    /// The installed version's file at the same path, unchanged.
    Kept(KeptFile),
}

/// `{"base64": "..."}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Base64File {
    /// The file's bytes, standard base64.
    pub base64: String,
}

/// `{"keep": "<sha256 hex>"}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeptFile {
    /// The SHA-256 of the installed file, in hex.
    pub keep: String,
}

/// A pushed version that is installed.
#[derive(Debug)]
pub struct PushedInstall {
    /// The digest of its files, which names its directory.
    pub digest: String,
    /// What the installer did.
    pub result: InstallResult,
    /// Each spec's check, by its path in the app.
    pub checks: BTreeMap<String, CascadeResult>,
}

/// Why a push was refused. Nothing it would have changed changed.
#[derive(Debug)]
pub enum PushRefused {
    /// The request is wrong: a name, a path, a kept file, or files that are
    /// not the app named.
    Request(String),
    /// A spec failed its check or ran out of its budget; every spec's check,
    /// by its path in the app.
    Checks(BTreeMap<String, CascadeResult>),
    /// The installer refused the version, saying why.
    Install(String),
    /// This server could not do its part, such as writing the files.
    Server(String),
}

/// Check and install `files` as `tenant`'s version of `app`, and say how long
/// the push waited for this host's turn ([`temper_server::host_turn`]).
///
/// Every spec is checked, one at a time, each within `budget`; a spec whose
/// levels read what an earlier passing check read gets that verdict back at
/// once. The whole push runs in the turn, so pushes and checks on this host,
/// in this server or a sibling, run one at a time.
pub async fn install_pushed_app(
    state: &PlatformState,
    tenant: &str,
    app: &str,
    files: BTreeMap<String, PushedFile>,
    budget: Duration,
) -> (Duration, Result<PushedInstall, PushRefused>) {
    let home = match files::app_home(&state.server, tenant, app) {
        Ok(home) => home,
        Err(error) => return (Duration::ZERO, Err(PushRefused::Request(error))),
    };
    let asked = Instant::now();
    let turn = temper_server::host_turn::take().await;
    let waited = asked.elapsed();
    let result = match turn {
        Ok(_turn) => install_in_turn(state, tenant, app, &home, files, budget).await,
        Err(error) => Err(PushRefused::Server(error)),
    };
    (waited, result)
}

async fn install_in_turn(
    state: &PlatformState,
    tenant: &str,
    app: &str,
    home: &Path,
    files: BTreeMap<String, PushedFile>,
    budget: Duration,
) -> Result<PushedInstall, PushRefused> {
    let prior = files::current(home);
    let kept_from = prior.as_ref().map(|digest| home.join(digest).join(app));
    let files = files::resolve(files, kept_from.as_deref())?;
    let digest = files::digest(&files);

    // A version already on disk holds exactly these files: its directory got
    // its name only once they were all written.
    let existing = home.join(&digest);
    let created = !existing.is_dir();
    let root = if created {
        files::materialize(home, &digest, app, &files).map_err(|error| {
            PushRefused::Server(format!("the pushed files could not be written: {error}"))
        })?
    } else {
        existing
    };
    let push = Push {
        tenant,
        app,
        home,
        root: &root,
        prior: prior.as_deref(),
        created,
        registered: prior.as_deref() == Some(digest.as_str()),
    };

    let app_dir = root.join(app);
    let named = read_app_manifest(&app_dir).is_some_and(|manifest| manifest.name == app);
    if !named || read_app_guide(&app_dir).is_none() {
        push.undo(false);
        return Err(PushRefused::Request(format!(
            "these files are not the app '{app}': it needs an app.toml naming '{app}' and an APP.md"
        )));
    }

    let checks = match check_specs(state, &app_dir, &files, budget).await {
        Ok(checks) => checks,
        Err(refused) => {
            push.undo(false);
            return Err(refused);
        }
    };
    if checks
        .values()
        .any(|check| !check.all_passed || check.stopped.is_some())
    {
        push.undo(false);
        return Err(PushRefused::Checks(checks));
    }

    // Order: record the checked version as current, then make it resolve and
    // install it, and name the previous version again if the install fails.
    // A crash after the record restarts on this version, which passed its
    // checks; the caller never got an answer, pushes again, and the same push
    // installs nothing new. Recording after the install would instead let a
    // crash between the two restart on the previous version while this one
    // had already gone live.
    if !push.registered
        && let Err(error) = files::set_current(home, Some(&digest))
    {
        push.undo(false);
        return Err(PushRefused::Server(format!(
            "the pushed version of '{app}' could not be recorded, so it was not installed: {error}"
        )));
    }

    add_tenant_os_apps_dir(
        tenant,
        root.clone(),
        DirApps::Only(BTreeSet::from([app.to_string()])),
    );
    let resolved = catalog()
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .app_dir(Some(tenant), app)
        .cloned();
    if resolved.as_ref() != Some(&app_dir) {
        push.undo(true);
        return Err(PushRefused::Server(format!(
            "the pushed version of '{app}' did not become the one tenant '{tenant}' resolves"
        )));
    }

    let result = match crate::install_os_app(state, tenant, app).await {
        Ok(result) => result,
        Err(error) => {
            push.undo(true);
            return Err(PushRefused::Install(error));
        }
    };
    // The same line a boot install prints: whoever reads this server's
    // output reads a pushed install the same way.
    println!("{}", installed_line(app, tenant, &result));

    if !push.registered {
        let keep: Vec<&str> = [Some(digest.as_str()), prior.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        files::prune(home, &keep, |old| remove_tenant_os_apps_dir(tenant, old));
    }
    Ok(PushedInstall {
        digest,
        result,
        checks,
    })
}

/// The line this server prints when it installs `app` for `tenant`, at boot
/// or by push: `  App '<app>' installed for '<tenant>': <types>`. Katagami's
/// control plane judges an app served by finding `App '<app>' installed for
/// '<tenant>'` in this server's output, so those words do not change.
pub fn installed_line(app: &str, tenant: &str, result: &InstallResult) -> String {
    let types: Vec<&str> = result
        .added
        .iter()
        .chain(&result.updated)
        .chain(&result.skipped)
        .map(String::as_str)
        .collect();
    format!(
        "  App '{app}' installed for '{tenant}': {}",
        types.join(", ")
    )
}

/// The line this server prints at boot for an `--app` it has no files for
/// anywhere: it serves without the app until the app's first install. Read by
/// Katagami's control plane, like [`installed_line`].
pub fn waiting_line(app: &str, tenant: &str) -> String {
    format!("  App '{app}' waits for its first install for '{tenant}'")
}

/// Make the version each app installed last by push resolve for its tenant
/// again, ahead of the shared catalog, so the boot install of the app installs
/// it rather than the app's files in the catalog. Returns how many it restored.
pub fn restore_pushed_app_roots(state: &PlatformState) -> usize {
    let roots = files::installed_roots(&state.server);
    for (tenant, app, root) in &roots {
        add_tenant_os_apps_dir(
            tenant,
            root.clone(),
            DirApps::Only(BTreeSet::from([app.clone()])),
        );
    }
    roots.len()
}

/// One push in progress, and how to take it back.
struct Push<'a> {
    tenant: &'a str,
    app: &'a str,
    /// The directory of the app's pushed versions.
    home: &'a Path,
    root: &'a Path,
    /// The version current before this push, if any.
    prior: Option<&'a str>,
    /// This push wrote the version's directory.
    created: bool,
    /// The version is the one installed now, so the tenant resolved it before.
    registered: bool,
}

impl Push<'_> {
    /// Leave things as they were before this push: the previous version is
    /// current again, the tenant resolves the app as it did, and a directory
    /// this push wrote is gone. `recorded` says this push named its version
    /// current and added it to the tenant's installs.
    fn undo(&self, recorded: bool) {
        if recorded && !self.registered {
            if let Err(error) = files::set_current(self.home, self.prior) {
                tracing::error!(
                    app = self.app,
                    tenant = self.tenant,
                    prior = ?self.prior,
                    %error,
                    "a refused pushed version is still recorded as current; a restart would \
                     install it"
                );
            }
            remove_tenant_os_apps_dir(self.tenant, self.root);
        }
        if self.created
            && let Err(error) = std::fs::remove_dir_all(self.root)
        {
            tracing::warn!(
                app = self.app,
                path = %self.root.display(),
                %error,
                "a refused pushed version could not be deleted"
            );
        }
    }
}

/// Check every spec the installer will read from `app_dir`, by its path.
async fn check_specs(
    state: &PlatformState,
    app_dir: &Path,
    files: &BTreeMap<String, Vec<u8>>,
    budget: Duration,
) -> Result<BTreeMap<String, CascadeResult>, PushRefused> {
    let mut checks = BTreeMap::new();
    for (_, path) in find_ioa_files(app_dir) {
        let relative = spec_path(app_dir, &path)?;
        let source = files
            .get(&relative)
            .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
            .ok_or_else(|| PushRefused::Request(format!("'{relative}' is not UTF-8 text")))?;
        let check = temper_server::observe::check_ioa(
            &state.server,
            source,
            temper_server::observe::DEFAULT_SIM_SEEDS,
            temper_server::observe::DEFAULT_PROP_TEST_CASES,
            budget,
        )
        .await
        .map_err(PushRefused::Server)?;
        checks.insert(relative, check);
    }
    Ok(checks)
}

fn spec_path(app_dir: &Path, path: &Path) -> Result<String, PushRefused> {
    path.strip_prefix(app_dir)
        .ok()
        .and_then(Path::to_str)
        .map(str::to_string)
        .ok_or_else(|| PushRefused::Server(format!("spec '{}' is outside its app", path.display())))
}
