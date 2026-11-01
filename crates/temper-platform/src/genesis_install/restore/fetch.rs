//! Fetching the installs a boot found neither on disk nor in a cache, without
//! holding the boot.
//!
//! Each stored install is independent of the others, and a registry can be
//! slow for one bundle (a large closure) or not answer at all. So each fetch
//! is its own task: one that stalls costs no other, and the boot goes on while
//! they run. A fetch that lands after the boot has recovered its apps brings
//! its own apps up, so an app whose files arrive late still runs. A fetch that
//! fails says so at WARN, and when all are done a summary says how many were
//! restored and how many were not.

use std::path::PathBuf;
use std::sync::Arc;

use temper_server::platform_store::{InstalledAppRecord, PlatformStore};
use tokio::task::{JoinHandle, JoinSet};

use super::super::registry::RegistryUrl;
use super::super::*;
use super::cached_apps_matching_records;
use crate::recovery::{InstalledAppRuntimeRecoveryOutcome, recover_installed_app_runtime_state};

/// A stored install whose files must be fetched again from its registry.
pub(super) struct Pending {
    pub(super) tenant: String,
    pub(super) record: InstalledAppRecord,
    pub(super) registry: RegistryUrl,
    pub(super) registry_tenant: String,
    pub(super) root_ref: RegistryAppRef,
    pub(super) cache_root: PathBuf,
}

/// What the fetches a boot started came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheFetchSummary {
    /// Installs whose files were fetched again and registered.
    pub fetched: usize,
    /// Installs whose files could not be fetched, or did not hold the
    /// recorded version. Their records are kept.
    pub failed: usize,
}

/// The fetches a boot started. They run whether or not anyone waits.
pub struct CacheFetches(Option<JoinHandle<CacheFetchSummary>>);

impl CacheFetches {
    /// No fetches.
    pub(super) fn none() -> Self {
        Self(None)
    }

    /// Wait for every fetch to end.
    pub async fn finished(self) -> CacheFetchSummary {
        match self.0 {
            Some(handle) => handle.await.unwrap_or_else(|error| {
                tracing::error!(error = %error, "Genesis cache recovery task did not finish");
                CacheFetchSummary::default()
            }),
            None => CacheFetchSummary::default(),
        }
    }
}

/// Start a task for each of `pending`, and one that reports when all end.
pub(super) fn spawn(
    platform: &PlatformState,
    ps: Arc<dyn PlatformStore>,
    pending: Vec<Pending>,
) -> CacheFetches {
    if pending.is_empty() {
        return CacheFetches::none();
    }
    let platform = platform.clone();
    CacheFetches(Some(tokio::spawn(async move {
        let mut fetches = JoinSet::new();
        for install in pending {
            fetches.spawn(fetch_and_bring_up(platform.clone(), ps.clone(), install));
        }
        let mut summary = CacheFetchSummary::default();
        while let Some(done) = fetches.join_next().await {
            match done {
                Ok(true) => summary.fetched += 1,
                Ok(false) => summary.failed += 1,
                Err(error) => {
                    summary.failed += 1;
                    tracing::error!(
                        recovery = "genesis_cache",
                        result = "error",
                        error = %error,
                        "Genesis cache fetch task ended without finishing"
                    );
                }
            }
        }
        if summary.failed > 0 {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                fetched = summary.fetched,
                failed = summary.failed,
                "Genesis cache recovery could not fetch every installed app; their install records are kept"
            );
        } else {
            tracing::info!(
                recovery = "genesis_cache",
                result = "ok",
                fetched = summary.fetched,
                "Genesis cache recovery fetched every installed app"
            );
        }
        summary
    })))
}

/// Fetch one install's files, register them, and bring its apps up. Whether
/// its files were fetched and registered.
async fn fetch_and_bring_up(
    platform: PlatformState,
    ps: Arc<dyn PlatformStore>,
    install: Pending,
) -> bool {
    let Pending {
        tenant,
        record,
        registry,
        registry_tenant,
        root_ref,
        cache_root,
    } = install;
    let app_name = record.app_name.clone();
    let materialized = match materialize_registry_app_closure_via_bundle(
        &registry,
        &registry_tenant,
        root_ref.clone(),
        &cache_root,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(error) if genesis_git_fallback_enabled() => {
            tracing::warn!(
                tenant = %tenant,
                app = %app_name,
                app_ref = %record.app_ref,
                registry_url = %registry.as_str(),
                error = %error,
                "Genesis bundle cache recovery failed; falling back to git clone because TEMPER_GENESIS_INSTALL_GIT_FALLBACK is enabled"
            );
            materialize_registry_app_closure(&registry, root_ref, &cache_root)
                .await
                .map(|_| ())
        }
        Err(error) => Err(error),
    };
    if let Err(error) = materialized {
        tracing::warn!(
            recovery = "genesis_cache",
            result = "error",
            tenant = %tenant,
            app = %app_name,
            app_ref = %record.app_ref,
            error = %error,
            "Failed to fetch installed Genesis app files again; its install record is kept"
        );
        return false;
    }
    legacy_specs::convert_legacy_bundle_specs(&cache_root);
    let recorded = std::slice::from_ref(&record);
    let Some(apps) =
        cached_apps_matching_records(ps.as_ref(), &cache_root, &record, recorded).await
    else {
        tracing::warn!(
            recovery = "genesis_cache",
            result = "error",
            tenant = %tenant,
            app = %app_name,
            app_ref = %record.app_ref,
            "Fetched Genesis app files do not hold the recorded version; its install record is kept"
        );
        return false;
    };
    let names: Vec<String> = apps.iter().map(|(app, _)| app.clone()).collect();
    register_written_apps(&tenant, cache_root, apps);
    for name in &names {
        bring_up(&platform, ps.as_ref(), &tenant, name).await;
    }
    let installed: Vec<(String, String)> = names
        .into_iter()
        .map(|name| (tenant.clone(), name))
        .collect();
    crate::os_apps::converge_installed_schemas(&platform, &installed).await;
    true
}

/// Bring installed app `app_name` up from the files just registered: restore
/// its runtime state when its stored install matches them, and replay its
/// install otherwise. An app in the fetched closure with no install record of
/// its own is left to whoever installs it.
async fn bring_up(platform: &PlatformState, ps: &dyn PlatformStore, tenant: &str, app_name: &str) {
    match ps.get_installed_app(tenant, app_name).await {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                tenant,
                app = %app_name,
                error = %error,
                "Could not read the install record of a fetched Genesis app; it is not brought up"
            );
            return;
        }
    }
    let outcome = recover_installed_app_runtime_state(platform, ps, tenant, app_name).await;
    match outcome {
        InstalledAppRuntimeRecoveryOutcome::Ready | InstalledAppRuntimeRecoveryOutcome::Healed => {}
        InstalledAppRuntimeRecoveryOutcome::NeedsReconcile => {
            if let Err(error) = crate::os_apps::restore_os_app(platform, tenant, app_name).await {
                tracing::warn!(
                    recovery = "genesis_cache",
                    result = "error",
                    tenant,
                    app = %app_name,
                    error = %error,
                    "A fetched Genesis app's stored install did not replay"
                );
            }
        }
        InstalledAppRuntimeRecoveryOutcome::MissingBundle
        | InstalledAppRuntimeRecoveryOutcome::StoreError => {
            tracing::warn!(
                recovery = "genesis_cache",
                result = "error",
                tenant,
                app = %app_name,
                outcome = ?outcome,
                "A fetched Genesis app could not be brought up"
            );
        }
    }
}
