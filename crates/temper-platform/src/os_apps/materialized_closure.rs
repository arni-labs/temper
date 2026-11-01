//! Resolve an incoming directory over the tenant's catalog without registering it.

use std::path::{Path, PathBuf};

use super::app_catalog::AppCatalog;
use super::reconcile::resolve_os_app_install_order_with_dependencies;
use super::{dependencies_for_entry, os_app_dependencies, tenant_os_app_dir};

/// Every app reconciliation will read, in dependency-first order, together
/// with the directory its pre-install checks must read. Incoming apps win
/// over the tenant's existing catalog, just as they do after registration.
pub(crate) fn resolve_materialized_app_closure(
    tenant: &str,
    cache_root: &Path,
    roots: &[String],
) -> Result<Vec<(String, PathBuf)>, String> {
    let incoming = AppCatalog::from_dir(cache_root.to_path_buf());
    for name in roots {
        if incoming.app_dir(None, name).is_none() {
            return Err(format!("incoming install has no OS app '{name}'"));
        }
    }
    let order = resolve_os_app_install_order_with_dependencies(roots, |name| {
        match incoming.app_entry(None, name) {
            Some(entry) => dependencies_for_entry(name, Some(entry)),
            None => os_app_dependencies(Some(tenant), name),
        }
    })?;
    order
        .into_iter()
        .map(|name| {
            let dir = incoming
                .app_dir(None, &name)
                .cloned()
                .or_else(|| tenant_os_app_dir(tenant, &name))
                .ok_or_else(|| {
                    format!("OS app '{name}' not found while resolving install closure")
                })?;
            Ok((name, dir))
        })
        .collect()
}
