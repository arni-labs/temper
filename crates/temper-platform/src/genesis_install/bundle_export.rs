//! A registry's export of one bundle: its closure's trees, read back as files.

use std::collections::BTreeSet;
use std::path::Path;

use super::bundles::{GenesisBundleBudget, collect_bundle_files};
use super::cache_paths::{app_cache_dir, scratch_dir};
use super::{GenesisAppBundle, GenesisRegistryBundleApp};

/// Where an export's trees come from.
pub(super) trait MaterializeTree {
    /// Write `app`'s tree to `app_dir`, returning the paths it wrote.
    fn materialize<'a>(
        &'a self,
        app: &'a GenesisAppBundle,
        app_dir: &'a Path,
        budget: &'a mut GenesisBundleBudget,
    ) -> impl Future<Output = Result<BTreeSet<String>, String>> + Send + 'a;
}

/// Each app of `closure` as bundle files: `source` writes the app's tree to
/// the directory it is handed, and the files it names are read back.
///
/// The trees go under a directory of this export's own in `scratch_base`,
/// removed when it returns. The files leave in the response and nothing reads
/// the trees again, so a directory shared with another export of the same
/// bundle would only let one replace a tree the other is still reading.
pub(super) async fn export_closure_files(
    scratch_base: &Path,
    registry_tenant: &str,
    closure: Vec<GenesisAppBundle>,
    source: &impl MaterializeTree,
) -> Result<Vec<GenesisRegistryBundleApp>, String> {
    let scratch = scratch_dir(scratch_base, ".genesis-export-")?;
    let mut apps = Vec::new();
    let mut bundle_budget = GenesisBundleBudget::new();
    let mut materialization_budget = GenesisBundleBudget::new();
    for app in closure {
        let app_dir = app_cache_dir(scratch.path(), &app.name)?;
        let started = std::time::Instant::now();
        let paths = source
            .materialize(&app, &app_dir, &mut materialization_budget)
            .await?;
        let files = collect_bundle_files(&app_dir, &paths, &mut bundle_budget)?;
        tracing::info!(
            registry_tenant = %registry_tenant,
            app = %app.name,
            version_hash = %app.version_hash,
            duration_ms = started.elapsed().as_millis() as u64,
            files = files.len(),
            "Exported Genesis app bundle files"
        );
        apps.push(GenesisRegistryBundleApp {
            owner: app.owner,
            name: app.name,
            version_hash: app.version_hash.trim_start_matches('@').to_string(),
            files,
        });
    }
    Ok(apps)
}

#[cfg(test)]
#[path = "bundle_export_test.rs"]
mod tests;
