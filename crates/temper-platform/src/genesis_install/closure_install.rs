//! The one install every source of app files ends in.
//!
//! A source only puts an app closure's files in a cache directory and names
//! each app's files: the registry fetches a pinned Genesis closure, a push
//! writes the files its caller sent. From there every install is the same:
//! each spec is checked, the named files become what the tenant resolves, the
//! closure reconciles and is verified runtime-ready, and the install commits
//! with its records or goes back to the version installed before it. Nothing
//! here lists the directory the source just wrote.

use std::time::Duration;

use temper_verify::CascadeResult;

use super::*;
use crate::os_apps::{
    AppFiles, find_ioa_files, installed_line, local_os_app_dependency_name, read_app_manifest,
    tenant_dependency_files,
};

/// Where an installed closure came from, as its install records name it.
pub(super) struct ClosureSource<'a> {
    /// `genesis` for a registry install, `pushed` for a pushed one.
    pub(super) source_kind: &'a str,
    pub(super) registry_url: &'a str,
    pub(super) registry_tenant: &'a str,
    pub(super) follow_policy: &'a str,
}

/// An app closure whose files are in its cache directory, ready to install.
pub(super) struct MaterializedClosure<'a> {
    /// The ref the install was asked for, as messages name it.
    pub(super) app_ref: &'a str,
    /// The closure's root app.
    pub(super) root: RegistryAppRef,
    /// Every app the directory holds at its version, the root among them.
    pub(super) apps: Vec<MaterializedApp>,
    /// The dependencies of `apps` the source did not write, as the tenant
    /// resolves them now, each with its files. They are part of the closure
    /// for every phase but registering and recording: checked, snapshotted
    /// for rollback, reconciled and verified with the rest.
    pub(super) resolved: Vec<(String, AppFiles)>,
    pub(super) source: ClosureSource<'a>,
    /// Time each spec's check may take.
    pub(super) budget: Duration,
}

/// One app of a closure: its version, and the files its source wrote for it
/// at `<cache root>/<name>`.
pub(super) struct MaterializedApp {
    pub(super) version: RegistryAppRef,
    pub(super) files: AppFiles,
}

/// A closure installed and recorded.
pub(super) struct ClosureInstalled {
    /// What the installer did for the root app.
    pub(super) install: InstallResult,
    pub(super) closure_id: String,
    /// The names of every app in the closure.
    pub(super) materialized: Vec<String>,
    /// Each spec's check, by its path: the root app's relative to it, every
    /// other app's under its name.
    pub(super) checks: BTreeMap<String, CascadeResult>,
}

/// Why a closure was not installed.
pub(super) enum ClosureRefused {
    /// A spec failed its check or ran out of its budget. Nothing changed: the
    /// checks run before the directory is registered.
    Checks(BTreeMap<String, CascadeResult>),
    /// The installer or its verification refused the closure; the version
    /// installed before it, if any, is installed again. Says why.
    Failed(String),
}

/// Install the closure in `cache_root` for `tenant`.
///
/// The caller holds this host's install turn ([`temper_server::host_turn`]):
/// the checks walk state spaces sized from this host's free memory.
pub(super) async fn install_materialized_closure(
    platform: &PlatformState,
    tenant: &str,
    cache_root: &Path,
    closure: MaterializedClosure<'_>,
) -> Result<ClosureInstalled, ClosureRefused> {
    let materialized: Vec<String> = closure
        .apps
        .iter()
        .map(|app| app.version.name.clone())
        .collect();
    let whole: Vec<(String, AppFiles)> = closure
        .apps
        .iter()
        .map(|app| (app.version.name.clone(), app.files.clone()))
        .chain(closure.resolved.iter().cloned())
        .collect();
    let every: Vec<String> = whole.iter().map(|(name, _)| name.clone()).collect();

    // Checked before anything changes, so a refusal leaves the tenant as it was.
    let checks = check_closure_specs(platform, &closure.root.name, &whole, closure.budget)
        .await
        .map_err(ClosureRefused::Failed)?;
    if checks
        .values()
        .any(|check| !check.all_passed || check.stopped.is_some())
    {
        return Err(ClosureRefused::Checks(checks));
    }

    // Capture the previously-installed records for EVERY app in the closure BEFORE mutating the
    // catalog or store, so a failed install can be rolled back to them. reconcile overwrites the
    // durable records, so these reads must happen first. A store read error here is fatal: the
    // capture is the rollback safety net, so we fail before mutating rather than silently losing
    // the ability to roll back.
    let ps = platform_store(platform);
    let mut prior_records: Vec<InstalledAppRecord> = Vec::new();
    if let Some(ps) = &ps {
        for app_name in &every {
            match ps.get_installed_app(tenant, app_name).await {
                Ok(Some(record)) => prior_records.push(record),
                Ok(None) => {}
                Err(error) => {
                    return Err(ClosureRefused::Failed(format!(
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

    let written = closure
        .apps
        .iter()
        .map(|app| (app.version.name.clone(), app.files.clone()))
        .collect();
    register_written_apps(tenant, cache_root.to_path_buf(), written);

    let reconcile_started = Instant::now();
    let reconcile_result =
        reconcile_materialized_app_closure(platform, tenant, &closure.root.name).await;

    // Verify EVERY app in the reconciled closure reached runtime-ready — specs/policies/wasm
    // registered AND every app-required wasm module compiles. A reconcile error counts as
    // not-ready and routes through the same decision. With no platform store there is nothing to
    // verify or roll back against, so a successful reconcile commits (preserving pre-existing
    // store-less behavior).
    let new_version_ready = match (&reconcile_result, &ps) {
        (Ok(_), Some(ps)) => {
            verify_install_closure_runtime_ready(platform, ps.as_ref(), tenant, &every).await
        }
        (Ok(_), None) => true,
        _ => false,
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
            // Whoever reads this server's output reads every install the same way.
            println!("{}", installed_line(&closure.root.name, tenant, &install));
            Ok(ClosureInstalled {
                install,
                closure_id: root_closure_id,
                materialized,
                checks,
            })
        }
        InstallVerifyDecision::RollBackToPrevious => {
            let prior =
                prior_root_record.expect("RollBackToPrevious implies a prior installed record");
            let ps = ps.expect("RollBackToPrevious implies a platform store");
            // The reconcile's own error says why, such as a refused spec
            // change; only a reconcile that went through failed verification.
            let failed = match &reconcile_result {
                Err(error) => format!("failed: {error}"),
                Ok(_) => "failed runtime verification".to_string(),
            };
            if let Err(error) =
                rollback_to_prior(platform, ps.as_ref(), tenant, &prior, &prior_records).await
            {
                return Err(ClosureRefused::Failed(format!(
                    "Genesis install of {} {failed}\nRollback to {} also failed: {error}",
                    closure.app_ref, prior.app_ref
                )));
            }
            Err(ClosureRefused::Failed(format!(
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
                Err(ClosureRefused::Failed(format!(
                    "Genesis install of {} failed runtime verification and no previous install exists to roll back to",
                    closure.app_ref
                )))
            }
        },
    }
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

/// The dependencies `written` apps name that none of them is, with theirs,
/// as `tenant` resolves them now: the rest of the closure the install
/// reconciles. An error names a dependency the tenant does not resolve.
pub(super) fn resolved_dependencies(
    tenant: &str,
    written: &[(String, AppFiles)],
) -> Result<Vec<(String, AppFiles)>, String> {
    let is_written = |name: &str| written.iter().any(|(app, _)| app == name);
    let needed: Vec<String> = written
        .iter()
        .filter_map(|(_, files)| read_app_manifest(files.dir()))
        .flat_map(|manifest| manifest.dependencies)
        .filter_map(|dependency| local_os_app_dependency_name(&dependency))
        .filter(|name| !is_written(name))
        .collect();
    let resolved = tenant_dependency_files(tenant, &needed)?;
    Ok(resolved
        .into_iter()
        .filter(|(name, _)| !is_written(name))
        .collect())
}

/// Check every spec the installer will read from the closure, through the
/// budgeted cascade `POST /api/specs/validate-ioa` runs. A spec whose levels
/// read what an earlier passing check read gets that kept verdict back at once.
async fn check_closure_specs(
    platform: &PlatformState,
    root_app: &str,
    apps: &[(String, AppFiles)],
    budget: Duration,
) -> Result<BTreeMap<String, CascadeResult>, String> {
    let mut checks = BTreeMap::new();
    for (name, files) in apps {
        for path in find_ioa_files(files) {
            let key = if name == root_app {
                path.clone()
            } else {
                format!("{name}/{path}")
            };
            let source = files
                .read_text(&path)
                .map_err(|error| format!("spec '{key}': {error}"))?;
            let check = temper_server::observe::check_ioa(
                &platform.server,
                source,
                temper_server::observe::DEFAULT_SIM_SEEDS,
                temper_server::observe::DEFAULT_PROP_TEST_CASES,
                budget,
            )
            .await?;
            checks.insert(key, check);
        }
    }
    Ok(checks)
}
