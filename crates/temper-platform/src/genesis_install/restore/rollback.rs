//! Recover prior dependencies even when another install holds their files.

use super::*;

/// Restore every prior dependency the root cache no longer holds at its recorded digest.
pub(super) async fn restore_prior_dependencies(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    prior: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
    served: &BTreeSet<String>,
) -> Result<(), String> {
    for record in prior_records {
        if record.app_name == prior.app_name || served.contains(&record.app_name) {
            continue;
        }
        restore_dependency(platform, ps, record, prior_records)
            .await
            .map_err(|error| {
                format!(
                    "failed to restore prior dependency {} for rollback: {error}",
                    record.app_ref
                )
            })?;
    }
    Ok(())
}

async fn restore_dependency(
    platform: &PlatformState,
    ps: &dyn PlatformStore,
    record: &InstalledAppRecord,
    prior_records: &[InstalledAppRecord],
) -> Result<(), String> {
    let registry_tenant = if record.registry_tenant.trim().is_empty() {
        "default"
    } else {
        record.registry_tenant.trim()
    };
    let mut candidates =
        install_cache_candidates(platform, &record.tenant, registry_tenant, &record.app_ref);
    let destination = candidates[0].clone();
    let own = first_matching_install(ps, &candidates, record, prior_records).await;
    let stored = match own {
        Some((stored, _)) => stored,
        None => {
            // A dependency's app_ref names its version, not the closure that cached it.
            // Only this tenant's recorded installs contribute candidate directories; every
            // candidate must still read back at the dependency's prior bundle digest.
            let mut closures = prior_records.to_vec();
            for (tenant, name) in ps.list_all_installed_apps().await? {
                if tenant != record.tenant || closures.iter().any(|r| r.app_name == name) {
                    continue;
                }
                if let Some(installed) = ps.get_installed_app(&tenant, &name).await? {
                    closures.push(installed);
                }
            }
            for closure in closures {
                if closure.tenant != record.tenant || closure.source_kind != "genesis" {
                    continue;
                }
                candidates.extend(install_cache_candidates(
                    platform,
                    &record.tenant,
                    if closure.registry_tenant.trim().is_empty() {
                        "default"
                    } else {
                        closure.registry_tenant.trim()
                    },
                    &closure.app_ref,
                ));
            }
            match first_matching_install(ps, &candidates, record, prior_records).await {
                Some((stored, _)) => stored,
                None => {
                    let registry = recorded_registry(&record.registry_url)?;
                    let app_ref = parse_registry_app_ref(&record.app_ref)?;
                    materialize_registry_app_closure_via_bundle(
                        &registry,
                        registry_tenant,
                        app_ref,
                        &destination,
                    )
                    .await?;
                    legacy_specs::convert_legacy_bundle_specs(&destination);
                    cached_apps_matching_records(ps, &destination, record, prior_records)
                        .await
                        .ok_or_else(|| format!(
                            "re-materialized previous version {} does not hold its recorded bundle",
                            record.app_ref
                        ))?;
                    destination
                }
            }
        }
    };
    // Register just this dependency; siblings are restored from their own prior records.
    register_install_cache(
        &record.tenant,
        stored,
        DirApps::Only(BTreeSet::from([record.app_name.clone()])),
    );
    Ok(())
}
