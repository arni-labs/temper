//! Which app in a tenant owns each entity type and entity set.
//!
//! An app owns the entity types its CSDL declares and the specs that run
//! them. A tenant's CSDL is merged from every app it installs, and a type
//! runs one spec, so a spec its own app does not declare, or an entity set
//! two apps declare, leaves one app serving under the other's schema with
//! nothing saying so.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use temper_spec::csdl::{CsdlDocument, parse_csdl};

use super::app_catalog::catalog;
use super::app_files::AppFiles;
use super::types::{AppBundle, InstallWarning};
use crate::state::PlatformState;

/// What `app_name`'s bundle would leave without an owner, or with two: each
/// spec whose entity type its CSDL does not declare, and each entity set
/// another app installed in `tenant` already declares, named with that app.
pub(super) async fn ownership_problems(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<Vec<String>, String> {
    let csdl = bundle_csdl(bundle, app_name)?;
    let declared = declared_types(csdl.as_ref());
    let mut problems: Vec<String> = bundle
        .specs
        .iter()
        .filter(|(entity_type, _)| !declared.contains(entity_type))
        .map(|(entity_type, _)| {
            format!("spec {entity_type} has no entity type {entity_type} in the app's CSDL")
        })
        .collect();
    let sets = entity_sets(csdl.as_ref());
    if !sets.is_empty() {
        for (other, other_sets) in installed_entity_sets(state, tenant, app_name).await? {
            for set in sets.keys().filter(|set| other_sets.contains(*set)) {
                problems.push(format!(
                    "entity set {set} is already owned by app '{other}'"
                ));
            }
        }
    }
    Ok(problems)
}

/// Warnings for what the bundle declares but will not run: each CSDL entity
/// type with no spec in the app, which serves its schema with no state
/// machine, and each spec or CSDL file under `app_dir` the install does not
/// read, named with the file it reads in its place.
pub(super) fn unrun_declarations(
    app_dir: &Path,
    app_name: &str,
    bundle: &AppBundle,
) -> Result<Vec<InstallWarning>, String> {
    let csdl = bundle_csdl(bundle, app_name)?;
    let specs: BTreeSet<&str> = bundle.specs.iter().map(|(name, _)| name.as_str()).collect();
    let mut warnings: Vec<InstallWarning> = csdl
        .as_ref()
        .map(|csdl| unspecified_entity_types(csdl, &specs))
        .unwrap_or_default()
        .into_iter()
        .map(InstallWarning::entity_ownership)
        .collect();
    warnings.extend(
        AppFiles::find(app_dir)
            .unread_warnings(app_dir)
            .into_iter()
            .map(InstallWarning::entity_ownership),
    );
    Ok(warnings)
}

fn bundle_csdl(bundle: &AppBundle, app_name: &str) -> Result<Option<CsdlDocument>, String> {
    bundle
        .csdl
        .as_deref()
        .map(|text| {
            parse_csdl(text)
                .map_err(|e| format!("Failed to parse CSDL for os-app '{app_name}': {e}"))
        })
        .transpose()
}

/// One line for each entity type `csdl` gives a set and no spec in `specs`
/// runs: its records are served as schema with no state machine. Install
/// and `temper verify` both report these.
pub fn unspecified_entity_types(csdl: &CsdlDocument, specs: &BTreeSet<&str>) -> Vec<String> {
    entity_sets(Some(csdl))
        .into_iter()
        .filter(|(_, entity_type)| !specs.contains(entity_type.as_str()))
        .map(|(set, entity_type)| {
            format!(
                "entity type {entity_type} (set {set}) has no spec in the app, so it is \
                 served as schema with no state machine"
            )
        })
        .collect()
}

fn declared_types(csdl: Option<&CsdlDocument>) -> BTreeSet<String> {
    csdl.into_iter()
        .flat_map(|doc| &doc.schemas)
        .flat_map(|schema| &schema.entity_types)
        .map(|entity_type| entity_type.name.clone())
        .collect()
}

/// Entity set name to the unqualified name of its entity type.
fn entity_sets(csdl: Option<&CsdlDocument>) -> BTreeMap<String, String> {
    csdl.into_iter()
        .flat_map(|doc| &doc.schemas)
        .flat_map(|schema| &schema.entity_containers)
        .flat_map(|container| &container.entity_sets)
        .map(|set| {
            let entity_type = set
                .entity_type
                .rsplit('.')
                .next()
                .unwrap_or(&set.entity_type);
            (set.name.clone(), entity_type.to_string())
        })
        .collect()
}

/// The entity sets each other app installed in `tenant` declares, read from
/// its files in the catalog. An app whose files the catalog no longer holds
/// cannot be compared, so it is logged and left out.
async fn installed_entity_sets(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<Vec<(String, BTreeSet<String>)>, String> {
    let Some(store) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    else {
        return Ok(Vec::new());
    };
    let installed = store
        .list_all_installed_apps()
        .await
        .map_err(|e| format!("Failed to list the apps installed in tenant '{tenant}': {e}"))?;
    let mut owners = Vec::new();
    for (_, other) in installed
        .into_iter()
        .filter(|(row_tenant, other)| row_tenant == tenant && other != app_name)
    {
        let app_dir = catalog()
            .read()
            .unwrap()
            .app_dir(Some(tenant), &other)
            .cloned(); // ci-ok: infallible lock
        let Some(app_dir) = app_dir else {
            tracing::warn!(tenant, app = %other, "installed app is not in the catalog; its entity sets are not compared");
            continue;
        };
        let Some(text) = AppFiles::find(&app_dir)
            .csdl
            .and_then(|path| std::fs::read_to_string(path).ok())
        else {
            continue;
        };
        let Ok(csdl) = parse_csdl(&text) else {
            tracing::warn!(tenant, app = %other, "installed app's CSDL does not parse; its entity sets are not compared");
            continue;
        };
        owners.push((other, entity_sets(Some(&csdl)).into_keys().collect()));
    }
    Ok(owners)
}
