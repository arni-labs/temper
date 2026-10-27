//! An install never moves a dependency backwards unless it is asked to.
//!
//! Installing an app installs its whole dependency closure, each dependency at
//! the version the installing app pins. When a dependency is already installed
//! from Genesis at another version, the pin alone does not say which of the two
//! is newer, so before anything changes the installer asks the registry how
//! they relate:
//!
//! - the pin descends from the installed version: an upgrade, installed as before;
//! - the installed version descends from the pin: the newer one is kept, with no
//!   reconcile and no new record;
//! - neither, or the registry cannot tell: the whole install is refused.
//!
//! A caller who means to move a dependency to its pin anyway names it in
//! `downgrade_dependencies`. The app being installed is not a dependency: its
//! version is the one the caller chose.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use temper_server::platform_store::{InstalledAppRecord, PlatformStore};
use temper_server::state::ServerState;

use super::cache_paths::{app_cache_dir, validate_identity_component};
use super::version_compare::{
    GenesisVersionCompareResponse, VersionComparison, compare_genesis_app_versions,
};
use super::{BundleAudience, RegistryAppRef};

/// How long the installer waits for a registry's compare answer.
///
/// A registry that accepts the connection and never answers must not hold an
/// install open. The same 30 seconds the bundle transport allows between
/// chunks; a compare answer is one small JSON object.
const COMPARE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The most bytes the installer reads of a compare answer, so a broken or
/// hostile registry cannot make it buffer without end. The answer is one word;
/// 64 KiB is what the bundle transport reads of an error body.
const MAX_COMPARE_RESPONSE_BYTES: usize = 64 * 1024;

/// One app of an install's closure, at the version the closure pins.
pub(super) struct PinnedApp<'a> {
    pub(super) owner: &'a str,
    pub(super) name: &'a str,
    pub(super) version: &'a str,
}

/// A dependency the install left at its newer installed version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeptDependency {
    /// `owner/name` of the dependency.
    pub app: String,
    /// The installed version, which descends from the pin and stays.
    pub installed_version: String,
    /// The version the installing closure pinned.
    pub pinned_version: String,
    #[serde(skip)]
    pub(super) name: String,
}

/// Where the installer learns how two versions of an app relate.
pub(super) trait VersionAncestry {
    /// How `head` relates to `base` in the app's history.
    async fn compare(
        &self,
        owner: &str,
        name: &str,
        base: &str,
        head: &str,
    ) -> Result<VersionComparison, String>;
}

/// A remote registry's compare route, for installs from a registry URL.
pub(super) struct RegistryAncestry<'a> {
    pub(super) registry_url: &'a str,
    pub(super) registry_tenant: &'a str,
}

impl VersionAncestry for RegistryAncestry<'_> {
    async fn compare(
        &self,
        owner: &str,
        name: &str,
        base: &str,
        head: &str,
    ) -> Result<VersionComparison, String> {
        let url = format!(
            "{}/api/genesis/apps/{owner}/{name}/compare/{base}...{head}",
            self.registry_url.trim_end_matches('/')
        );
        let mut response = reqwest::Client::new()
            .get(&url)
            .header("X-Tenant-Id", self.registry_tenant)
            .timeout(COMPARE_RESPONSE_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("request {url}: {error}"))?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| format!("read {url}: {error}"))?
        {
            if body.len() + chunk.len() > MAX_COMPARE_RESPONSE_BYTES {
                return Err(format!(
                    "{url} answered more than {MAX_COMPARE_RESPONSE_BYTES} bytes"
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            return Err(format!("{url} returned {status}"));
        }
        serde_json::from_slice::<GenesisVersionCompareResponse>(&body)
            .map(|answer| answer.status)
            .map_err(|error| format!("decode {url}: {error}"))
    }
}

/// This Genesis's own commit rows, for the App.Install hook.
pub(super) struct StoredAncestry<'a> {
    pub(super) state: &'a ServerState,
    pub(super) registry_tenant: &'a str,
}

impl VersionAncestry for StoredAncestry<'_> {
    async fn compare(
        &self,
        owner: &str,
        name: &str,
        base: &str,
        head: &str,
    ) -> Result<VersionComparison, String> {
        // The hook runs after a governed App.Install, so the read is not
        // anonymous.
        compare_genesis_app_versions(
            self.state,
            self.registry_tenant,
            owner,
            name,
            base,
            head,
            BundleAudience::Authenticated,
        )
        .await
    }
}

/// Read the caller's `owner/name` list of dependencies it accepts moving to
/// the pin. An entry that is not `owner/name` is refused, not ignored.
pub(super) fn parse_downgrade_dependencies(entries: &[String]) -> Result<BTreeSet<String>, String> {
    let mut named = BTreeSet::new();
    for entry in entries {
        let entry = entry.trim();
        let parsed = entry.split_once('/').filter(|(owner, name)| {
            validate_identity_component("owner", owner).is_ok()
                && validate_identity_component("app name", name).is_ok()
        });
        if parsed.is_none() {
            return Err(format!(
                "downgrade_dependencies entry '{entry}' must be owner/name"
            ));
        }
        named.insert(entry.to_string());
    }
    Ok(named)
}

/// The App.Install hook's `DowngradeDependencies` field: a list of
/// `owner/name` strings, or one comma-separated string.
pub(super) fn downgrade_field(params: &Value) -> Vec<String> {
    match params
        .get("DowngradeDependencies")
        .or_else(|| params.get("downgrade_dependencies"))
    {
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| entry.as_str().unwrap_or_default().to_string())
            .collect(),
        Some(Value::String(joined)) => joined
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// The installed Genesis version a pin would replace, when the rule has to
/// look at history; `None` when the install goes ahead as before.
pub(super) fn version_to_compare<'r>(
    installed: Option<&'r InstalledAppRecord>,
    pinned: &str,
    downgrade_named: bool,
) -> Option<&'r str> {
    let record = installed.filter(|record| record.source_kind == "genesis")?;
    let current = record.version_hash.trim_start_matches('@');
    (!downgrade_named && !current.is_empty() && current != pinned.trim_start_matches('@'))
        .then_some(current)
}

/// What the install does with a dependency installed at another version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DependencyMove {
    /// The pin is newer: install it.
    Install,
    /// The installed version is newer: leave it as it is.
    Keep,
    /// Refuse the install, for this reason.
    Refuse(String),
}

/// The rule, given how the pin (head) relates to the installed version (base).
pub(super) fn dependency_move(comparison: &Result<VersionComparison, String>) -> DependencyMove {
    match comparison {
        Ok(VersionComparison::Ahead | VersionComparison::Identical) => DependencyMove::Install,
        Ok(VersionComparison::Behind) => DependencyMove::Keep,
        Ok(VersionComparison::Diverged) => {
            DependencyMove::Refuse("neither version descends from the other".to_string())
        }
        Ok(VersionComparison::Unknown) => {
            DependencyMove::Refuse("the registry could not tell how they relate".to_string())
        }
        Err(error) => DependencyMove::Refuse(format!("the registry's compare failed: {error}")),
    }
}

/// Decide every dependency of a closure before anything changes, and return
/// the ones to keep. Any refusal refuses the whole install, naming each
/// dependency with both versions.
pub(super) async fn plan_dependency_moves<A: VersionAncestry>(
    ancestry: &A,
    root_name: &str,
    closure: &[PinnedApp<'_>],
    installed: &[InstalledAppRecord],
    downgrade: &BTreeSet<String>,
) -> Result<Vec<KeptDependency>, String> {
    let mut kept = Vec::new();
    let mut refusals = Vec::new();
    for app in closure.iter().filter(|app| app.name != root_name) {
        let key = format!("{}/{}", app.owner, app.name);
        let record = installed.iter().find(|record| record.app_name == app.name);
        let Some(current) = version_to_compare(record, app.version, downgrade.contains(&key))
        else {
            continue;
        };
        let pinned = app.version.trim_start_matches('@');
        let comparison = ancestry.compare(app.owner, app.name, current, pinned).await;
        match dependency_move(&comparison) {
            DependencyMove::Install => {}
            DependencyMove::Keep => kept.push(KeptDependency {
                app: key,
                installed_version: current.to_string(),
                pinned_version: pinned.to_string(),
                name: app.name.to_string(),
            }),
            DependencyMove::Refuse(reason) => refusals.push(format!(
                "dependency {key} is installed at {current} and the closure pins {pinned}: {reason}"
            )),
        }
    }
    if refusals.is_empty() {
        return Ok(kept);
    }
    Err(format!(
        "refused before any change: {}. Name the dependency in downgrade_dependencies to move it to the pinned version.",
        refusals.join("; ")
    ))
}

/// Take the kept dependencies out of a materialized closure, so the app
/// catalog keeps resolving them to the version that is installed.
pub(super) fn drop_kept_from_cache(
    cache_root: &Path,
    kept: &[KeptDependency],
) -> Result<(), String> {
    for dependency in kept {
        let dir = app_cache_dir(cache_root, &dependency.name)?;
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|error| format!("remove kept dependency '{}': {error}", dir.display()))?;
        }
    }
    Ok(())
}

/// On recovery, take out of a closure restored for `root_name` every other
/// app this tenant runs at a different Genesis version than the closure pins.
/// Each app is then resolved from the closure its own record names.
pub(super) async fn drop_installed_elsewhere(
    ps: &dyn PlatformStore,
    tenant: &str,
    root_name: &str,
    closure: &[RegistryAppRef],
    cache_root: &Path,
) -> Result<(), String> {
    for app in closure.iter().filter(|app| app.name != root_name) {
        let Some(pinned) = app.version_hash.as_deref() else {
            continue;
        };
        let record = ps.get_installed_app(tenant, &app.name).await?;
        if version_to_compare(record.as_ref(), pinned, false).is_some() {
            let dir = app_cache_dir(cache_root, &app.name)?;
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|error| {
                    format!("remove '{}' from restored closure: {error}", dir.display())
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "dependency_versions_test.rs"]
mod tests;
