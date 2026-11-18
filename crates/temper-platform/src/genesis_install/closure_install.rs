//! The one install every source of app files ends in.
//!
//! A source only puts an app closure's files in a cache directory and names
//! each app's files: the registry fetches a pinned Genesis closure, a push
//! writes the files its caller sent. From there every install is the same:
//! a dependency already installed is left as it is ([`uninstalled_apps`]),
//! each spec is parsed, the named files become what the tenant resolves, the
//! closure reconciles and is verified runtime-ready, the routes each app
//! declares are set ([`routes`]), and the install commits with its records or
//! goes back, routes included, to the version installed before it. Nothing
//! here lists the directory the source just wrote.
//!
//! An install checks at main's level, as a TrustBundle install does: each
//! spec must parse, and the installer's joint proofs become warnings. The
//! per-spec L0-L3 cascade is not run here; a caller runs it before it
//! sends (an app builder, through `POST /api/specs/validate-ioa`).

use super::*;
use crate::os_apps::{
    AppFileSource, find_ioa_files, installed_line, resolve_materialized_app_closure,
    tenant_os_app_files, unheld_dependencies,
};

/// Where an installed closure came from, as its install records name it.
pub(super) struct ClosureSource<'a> {
    /// `genesis` for a registry install, `pushed` for a pushed one.
    pub(super) source_kind: &'a str,
    pub(super) registry_url: &'a str,
    pub(super) registry_tenant: &'a str,
    pub(super) follow_policy: &'a str,
}

/// A source's app files in its cache directory, ready to resolve and install.
pub(super) struct MaterializedClosure<'a> {
    /// The ref the install was asked for, as messages name it.
    pub(super) app_ref: &'a str,
    /// The closure's root app.
    pub(super) root: RegistryAppRef,
    /// Every app this source materialized at its version, the root among
    /// them. The install takes the root and each dependency not installed
    /// yet ([`uninstalled_apps`]); every other dependency resolves from the
    /// tenant catalog and keeps its own version and source.
    pub(super) apps: Vec<MaterializedApp>,
    pub(super) source: ClosureSource<'a>,
}

/// One app of a closure: its version, and the files its source wrote for it
/// at `<cache root>/<name>`.
pub(super) struct MaterializedApp {
    pub(super) version: RegistryAppRef,
    pub(super) files: AppFileSource,
}

/// A closure installed and recorded.
pub(super) struct ClosureInstalled {
    /// What the installer did for the root app.
    pub(super) install: InstallResult,
    pub(super) closure_id: String,
    /// The names of every app in the closure.
    pub(super) materialized: Vec<String>,
}

/// Why a closure was not installed.
pub(super) enum ClosureRefused {
    /// A spec does not parse; says which and why. Nothing changed: specs
    /// are parsed before the directory is registered.
    Unparsed(String),
    /// The installer or its verification refused the closure; the version
    /// installed before it, if any, is installed again. Says why.
    Failed(String),
    /// Refused before registration, or successfully rolled back. The caller
    /// may discard a newly created cache unless the prior install uses it.
    Inactive(String),
}

/// Why a check of a closure answers no.
#[derive(Debug)]
pub(super) enum CheckRefused {
    /// A spec does not parse; says which and why.
    Unparsed(String),
    /// The install would refuse it, in the install's words.
    Refused(String),
    /// The check cannot answer as the install would; says why.
    Unchecked(String),
}

/// Install the closure in `cache_root` for `tenant`.
///
/// The caller holds this host's install turn ([`temper_server::host_turn`]).
pub(super) async fn install_materialized_closure(
    platform: &PlatformState,
    tenant: &str,
    cache_root: &Path,
    mut closure: MaterializedClosure<'_>,
) -> Result<ClosureInstalled, ClosureRefused> {
    closure.apps = uninstalled_apps(
        platform,
        tenant,
        &closure.root.name,
        std::mem::take(&mut closure.apps),
        |app| &app.version.name,
    )
    .await
    .map_err(|error| {
        ClosureRefused::Inactive(format!(
            "Genesis install of {} aborted before mutating: {error}",
            closure.app_ref
        ))
    })?;
    // Keep the source's explicit paths, including through dependency
    // resolution: a fresh install never relies on listing its new directory.
    let written: Vec<(String, AppFileSource)> = closure
        .apps
        .iter()
        .map(|app| (app.version.name.clone(), app.files.clone()))
        .collect();
    hold_installed_dependencies(platform, tenant, cache_root, &written).await;
    let apps = resolve_materialized_app_closure(tenant, cache_root, &written)
        .map_err(ClosureRefused::Inactive)?;
    let materialized: Vec<String> = apps.iter().map(|(name, _)| name.clone()).collect();

    // One complete order drives the parse, snapshots, reconcile and verification.
    parse_closure_specs(&closure.root.name, &apps)?;
    // The routes each installed app declares now and declared before, read
    // before anything changes: a route file that does not read refuses here.
    let route_changes = route_changes(tenant, &written).map_err(ClosureRefused::Inactive)?;

    // Capture the previously-installed records for EVERY app in the closure BEFORE mutating the
    // catalog or store, so a failed install can be rolled back to them. reconcile overwrites the
    // durable records, so these reads must happen first. A store read error here is fatal: the
    // capture is the rollback safety net, so we fail before mutating rather than silently losing
    // the ability to roll back.
    let ps = platform_store(platform);
    let mut prior_records: Vec<InstalledAppRecord> = Vec::new();
    if let Some(ps) = &ps {
        for app_name in &materialized {
            match ps.get_installed_app(tenant, app_name).await {
                Ok(Some(record)) => prior_records.push(record),
                Ok(None) => {}
                Err(error) => {
                    return Err(ClosureRefused::Inactive(format!(
                        "Genesis install of {} aborted before mutating: could not read prior install record for {app_name} (rollback safety net): {error}",
                        closure.app_ref
                    )));
                }
            }
        }
    }
    let prior_root_record = prior_records
        .iter()
        .find(|record| record.app_name == closure.root.name)
        .cloned();

    register_written_apps(tenant, cache_root.to_path_buf(), written);

    let reconcile_started = Instant::now();
    let reconcile_result = reconcile_app_install_order(
        platform,
        tenant,
        &closure.root.name,
        &materialized,
        Swap::ModulesFirst,
    )
    .await;

    // Verify EVERY app in the reconciled closure reached runtime-ready — specs/policies/wasm
    // registered AND every app-required wasm module compiles. A reconcile error counts as
    // not-ready and routes through the same decision. With no platform store there is nothing to
    // verify or roll back against, so a successful reconcile commits (preserving pre-existing
    // store-less behavior).
    let new_version_ready = match (&reconcile_result, &ps) {
        (Ok(_), Some(ps)) => {
            verify_install_closure_runtime_ready(platform, ps.as_ref(), tenant, &materialized).await
        }
        (Ok(_), None) => true,
        _ => false,
    };
    // The routes go in last, once the version they call is ready; when they
    // cannot, they are put back and the install goes back with them.
    let mut routes_failed = None;
    let new_version_ready = new_version_ready
        && match routes::apply(platform, tenant, &route_changes).await {
            Ok(()) => true,
            Err(error) => {
                let error = match routes::put_back(platform, tenant, &route_changes).await {
                    Ok(()) => error,
                    Err(back) => format!("{error}; putting the routes back: {back}"),
                };
                routes_failed = Some(error);
                false
            }
        };

    match classify_install_verify(new_version_ready, prior_root_record.as_ref()) {
        InstallVerifyDecision::Commit => {
            let install = reconcile_result.expect("Commit implies a successful reconcile");
            log_genesis_install_phase(
                closure.app_ref,
                "install_reconcile",
                reconcile_started,
                materialized.len(),
                install.wasm_modules.len(),
            );
            let root_closure_id = record_closure(platform, tenant, &closure)
                .await
                .map_err(ClosureRefused::Failed)?;
            install.publish_reviews();
            // Whoever reads this server's output reads every install the same way.
            println!("{}", installed_line(&closure.root.name, tenant, &install));
            Ok(ClosureInstalled {
                install,
                closure_id: root_closure_id,
                materialized,
            })
        }
        InstallVerifyDecision::RollBackToPrevious => {
            let prior =
                prior_root_record.expect("RollBackToPrevious implies a prior installed record");
            let ps = ps.expect("RollBackToPrevious implies a platform store");
            // The reconcile's own error says why, such as a refused spec
            // change; only a reconcile that went through failed verification.
            let failed = match (&reconcile_result, &routes_failed) {
                (Err(error), _) => format!("failed: {error}"),
                (Ok(_), Some(error)) => format!("failed: {error}"),
                (Ok(_), None) => "failed runtime verification".to_string(),
            };
            if let Err(error) =
                rollback_to_prior(platform, ps.as_ref(), tenant, &prior, &prior_records).await
            {
                return Err(ClosureRefused::Failed(format!(
                    "Genesis install of {} {failed}\nRollback to {} also failed: {error}",
                    closure.app_ref, prior.app_ref
                )));
            }
            Err(ClosureRefused::Inactive(format!(
                "Genesis install of {} {failed}\nRolled back to previous pinned ref {}",
                closure.app_ref, prior.app_ref
            )))
        }
        InstallVerifyDecision::FailNoRollback => match reconcile_result {
            Err(error) => Err(ClosureRefused::Failed(format!(
                "Genesis install of {} failed: {error}",
                closure.app_ref
            ))),
            Ok(_) => {
                // The new version installed but is not runtime-ready and there is no prior version
                // to revert to. Mark the record failed so nothing treats the broken version as
                // healthy.
                if let Some(ps) = &ps {
                    mark_install_failed(ps.as_ref(), tenant, &closure.root.name).await;
                }
                let why = routes_failed.unwrap_or_else(|| "runtime verification".to_string());
                Err(ClosureRefused::Failed(format!(
                    "Genesis install of {} failed: {why}; no previous install exists to roll back to",
                    closure.app_ref
                )))
            }
        },
    }
}

/// What [`install_materialized_closure`] would answer for the closure in
/// `cache_root`, writing nothing: the same resolution and parse, then each
/// app's install checks against the tenant as it is
/// ([`check_app_install_order`]). The catalog keeps resolving every app to
/// the files it held. What only an installed version shows, that its required
/// modules compile once registered, is not checked.
pub(super) async fn check_materialized_closure(
    platform: &PlatformState,
    tenant: &str,
    cache_root: &Path,
    closure: &MaterializedClosure<'_>,
) -> Result<InstallResult, CheckRefused> {
    let written: Vec<(String, AppFileSource)> = closure
        .apps
        .iter()
        .map(|app| (app.version.name.clone(), app.files.clone()))
        .collect();
    let written = uninstalled_apps(platform, tenant, &closure.root.name, written, |app| &app.0)
        .await
        .map_err(CheckRefused::Unchecked)?;
    let refused = |error: String| {
        CheckRefused::Refused(format!(
            "Genesis install of {} failed: {error}",
            closure.app_ref
        ))
    };
    let apps = resolve_materialized_app_closure(tenant, cache_root, &written)
        .map_err(CheckRefused::Refused)?;
    parse_closure_specs(&closure.root.name, &apps).map_err(|refusal| match refusal {
        ClosureRefused::Unparsed(error) => CheckRefused::Unparsed(error),
        ClosureRefused::Failed(error) | ClosureRefused::Inactive(error) => {
            CheckRefused::Refused(error)
        }
    })?;
    route_changes(tenant, &written).map_err(refused)?;
    match check_app_install_order(platform, tenant, &closure.root.name, &apps).await {
        Err(CheckRefused::Refused(error)) => Err(refused(error)),
        other => other,
    }
}

/// The apps of `root`'s closure that installing `root` puts in place: `root`
/// itself, and each dependency of which no version is installed in `tenant`.
///
/// Installing an app never replaces or downgrades an installed dependency.
/// Other installed apps run on that dependency too, and a closure often pins
/// an older version than the tenant runs: on 17 Nov 2026 one app's pinned
/// install replaced the dependency another app's releases needed with an
/// older copy, and those releases stopped. So a dependency's pin names the
/// version to bring when none is installed, not the one that must run, and a
/// dependency is upgraded by installing it. Installed means the tenant has an
/// install record for the app's name, whatever its source, version or status,
/// as TemperPaw's boot decides for its bootstrap apps: the record is what boot
/// restores and what a failed install rolls back to. A dependency left out
/// resolves from the tenant's catalog, at the version its record names. With
/// no platform store there are no records, and every app is taken.
pub(super) async fn uninstalled_apps<T>(
    platform: &PlatformState,
    tenant: &str,
    root: &str,
    apps: Vec<T>,
    name: impl Fn(&T) -> &str,
) -> Result<Vec<T>, String> {
    let Some(ps) = platform_store(platform) else {
        return Ok(apps);
    };
    let mut taken = Vec::with_capacity(apps.len());
    for app in apps {
        let app_name = name(&app);
        let installed = app_name != root
            && ps
                .get_installed_app(tenant, app_name)
                .await
                .map_err(|error| {
                    format!("could not read the install record of dependency {app_name}: {error}")
                })?
                .is_some();
        if !installed {
            taken.push(app);
        }
    }
    Ok(taken)
}

/// Bring back the files of each installed app the closure of `written`
/// depends on that this process does not hold, so the closure resolves.
///
/// A dependency left installed resolves from the files this process holds for
/// its recorded version. A boot whose registry did not answer holds none, and
/// every install needing that app would be refused. So the install brings them
/// back first, as a boot does; resolution names any still missing. The caller
/// holds the install turn.
async fn hold_installed_dependencies(
    platform: &PlatformState,
    tenant: &str,
    cache_root: &Path,
    written: &[(String, AppFileSource)],
) {
    let mut asked = BTreeSet::new();
    loop {
        let unheld: Vec<String> = unheld_dependencies(tenant, cache_root, written)
            .into_iter()
            .filter(|app| asked.insert(app.clone()))
            .collect();
        if unheld.is_empty() {
            return;
        }
        // Boxed: bringing an app back can replay its install, a future like this one.
        Box::pin(super::restore::hold_installed_apps_in_turn(
            platform, tenant, &unheld,
        ))
        .await;
    }
}

/// For each app the install puts in place, the routes it declares and the
/// routes the version the tenant resolves now declared.
fn route_changes(
    tenant: &str,
    written: &[(String, AppFileSource)],
) -> Result<Vec<routes::RouteChange>, String> {
    written
        .iter()
        .map(|(app, files)| {
            let before = match tenant_os_app_files(tenant, app) {
                Some(installed) => routes::declared_routes(app, &installed).unwrap_or_else(|error| {
                    tracing::warn!(tenant, app = %app, %error, "the installed version's routes do not read; none are taken as its");
                    Vec::new()
                }),
                None => Vec::new(),
            };
            Ok(routes::RouteChange {
                app: app.clone(),
                before,
                now: routes::declared_routes(app, files)?,
            })
        })
        .collect()
}

/// Record every app of an installed closure with where it came from, and
/// return the root's closure id.
///
/// The ROOT app's write is load-bearing: if it is silently dropped, startup
/// reads a local record and installs whatever its catalog holds instead of
/// this version. A failed root write fails the install; a dependency's is
/// best-effort (a warning).
pub(super) async fn record_closure(
    platform: &PlatformState,
    tenant: &str,
    closure: &MaterializedClosure<'_>,
) -> Result<String, String> {
    let root_hash = closure.root.version_hash.as_deref().unwrap_or_default();
    let root_closure_id = format!(
        "{}:{}:{}",
        closure.source.source_kind,
        closure.app_ref,
        root_hash.trim_start_matches('@')
    );
    for materialized_ref in closure.apps.iter().map(|app| &app.version) {
        let Some(version_hash) = materialized_ref.version_hash.as_deref() else {
            continue;
        };
        let version_hash = version_hash.trim_start_matches('@');
        let app_ref = format!(
            "{}/{}@{version_hash}",
            materialized_ref.owner, materialized_ref.name
        );
        let is_root = materialized_ref.owner == closure.root.owner
            && materialized_ref.name == closure.root.name;
        let closure_id = if is_root {
            root_closure_id.clone()
        } else {
            format!("{}:{app_ref}:{version_hash}", closure.source.source_kind)
        };
        let result = record_genesis_install_metadata(
            platform,
            GenesisInstallMetadata {
                target_tenant: tenant,
                app_name: &materialized_ref.name,
                app_ref: &app_ref,
                version_hash,
                closure_id: &closure_id,
                source_kind: closure.source.source_kind,
                registry_url: closure.source.registry_url,
                registry_tenant: closure.source.registry_tenant,
                follow_policy: closure.source.follow_policy,
            },
        )
        .await;
        match result {
            Err(error) if is_root => {
                return Err(format!(
                    "Genesis install of {} reconciled but could not persist provenance: {error}",
                    closure.app_ref
                ));
            }
            Err(error) => tracing::warn!(
                app = %materialized_ref.name,
                error = %error,
                "Failed to persist dependency Genesis provenance during install"
            ),
            Ok(()) => {}
        }
    }
    Ok(root_closure_id)
}

/// Parse every spec the installer will read from the closure, before anything
/// changes: a spec that does not parse refuses the install, naming it, as the
/// installer itself would after registering it.
fn parse_closure_specs(
    root_app: &str,
    apps: &[(String, AppFileSource)],
) -> Result<(), ClosureRefused> {
    for (name, files) in apps {
        for path in find_ioa_files(files) {
            let key = if name == root_app {
                path.clone()
            } else {
                format!("{name}/{path}")
            };
            let source = files
                .read_text(&path)
                .map_err(|error| ClosureRefused::Inactive(format!("spec '{key}': {error}")))?;
            if let Err(error) = temper_spec::automaton::parse_automaton(&source) {
                return Err(ClosureRefused::Unparsed(format!(
                    "spec '{key}' does not parse: {error}"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "closure_install_tests.rs"]
mod tests;
