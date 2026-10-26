//! Checks an os-app install makes before it writes anything.
//!
//! A bundle that cannot be registered must leave no stored row for the next
//! start to restore, so these refuse it before the first write.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_spec::cross_invariant;
use temper_verify::composite::{JointOutcome, LivenessOutcome};

use super::InstallWarning;

use crate::state::PlatformState;

/// Refuse an install into a tenant whose stored specs did not load at startup.
///
/// An install rewrites the tenant's spec and constraints rows; for such a
/// tenant those stored rows are what needs repair, so they are kept as they are.
pub(super) fn refuse_unavailable_tenant(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
) -> Result<(), String> {
    if state.tenant_unavailable_reason(tenant).is_none() {
        return Ok(());
    }
    Err(format!(
        "tenant '{tenant}' is unavailable (its stored specs did not load at startup), so \
         os-app '{app_name}' is not installed and the stored rows are kept for repair"
    ))
}

/// Parse the bundle's cross-invariants, if it has any, so a bundle whose
/// cross-invariants would not load is refused with this error.
pub(super) fn parse_cross_invariants(source: Option<&str>, app_name: &str) -> Result<(), String> {
    let Some(source) = source.filter(|source| !source.trim().is_empty()) else {
        return Ok(());
    };
    cross_invariant::parse_cross_invariants(source)
        .map(|_| ())
        .map_err(|e| format!("Failed to parse cross-invariants for os-app '{app_name}': {e}"))
}

/// Time and states the joint check of one install may spend. Anyone who may
/// install an app can send specs whose joint model never ends: unbounded,
/// one install would hold a core and gigabytes for hours. These are
/// validate-ioa's caps for the same attack (its `MAX_BUDGET_MS` and
/// `BUDGET_STATES`). The time cap comes first: a liveness walk of the
/// Katagami control specs reached 2.2M states in 120 s and held 1.5 GB on a
/// Mac. A check that runs out leaves its rule unproved, which is a warning:
/// the install goes ahead. Intermediate expansion work also spends the state
/// allowance.
const JOINT_INSTALL_BUDGET: temper_verify::Budget = temper_verify::Budget {
    time: std::time::Duration::from_secs(120),
    states: 10_000_000,
};

/// Prove every `[[invariant]]` and `[[liveness]]` of `specs` (a bundle's
/// types) that reads a related record, on the joint model of its type and
/// the types it names, with `policies` (the tenant's Cedar as the install
/// leaves it) deciding each move as its sender. Both run when the install
/// brings `new_specs` (added or changed types) or policies: who Cedar lets
/// send each move decides whether a rule holds and whether a record gets
/// where it should. A named type the bundle does not carry is taken from what
/// the tenant already runs. Policy and spec changes check every active tenant
/// spec, overlaid with the incoming specs.
///
/// Only a rule the check finds broken refuses the install: an invariant with
/// a path of moves that breaks it, or a liveness rule with a path to a state
/// from which no move reaches its condition. A rule it could not prove (out
/// of budget, or reading a type nobody declares) is a warning and the
/// install goes ahead; those rules are returned for the install's answer.
/// It runs before the install writes or swaps anything, so a refused bundle
/// leaves the tenant as it was and reads keep serving the old specs
/// meanwhile, and on a blocking thread, since a large joint model takes
/// seconds.
pub(super) async fn prove(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    new_specs: bool,
    policies: Option<&str>,
) -> Result<Vec<String>, String> {
    if !new_specs && policies.is_none() {
        return Ok(Vec::new());
    }
    // A liveness property's own type counts as named, so one that reads no
    // other type is still checked.
    let Some(automatons) = bundle_with_named(state, tenant, app_name, specs, |a| {
        let invariants = a
            .invariants
            .iter()
            .flat_map(|inv| inv.assert.related_types());
        let liveness = a
            .liveness
            .iter()
            .filter_map(|live| live.eventually.as_ref())
            .flat_map(|goal| {
                let own = (a.automaton.name.clone(), String::new());
                goal.related_types().into_iter().chain(std::iter::once(own))
            });
        invariants.chain(liveness).collect()
    })?
    else {
        return Ok(Vec::new());
    };
    let policy_text = policies.map_or_else(
        || super::cached_or_active_tenant_policy_text(state, tenant),
        str::to_string,
    );
    let policies = temper_authz::SendAnalysis::new(&policy_text)
        .map_err(|e| format!("os-app '{app_name}': its Cedar could not be read: {e}"))?;
    let (invariants, liveness) = tokio::task::spawn_blocking(move || {
        let refs: Vec<&temper_spec::automaton::Automaton> = automatons.values().collect();
        (
            temper_verify::composite::verify_joint_invariants(
                &refs,
                Some(&policies),
                JOINT_INSTALL_BUDGET,
            ),
            temper_verify::composite::verify_joint_liveness(
                &refs,
                Some(&policies),
                JOINT_INSTALL_BUDGET,
            ),
        )
    })
    .await
    .map_err(|e| format!("os-app '{app_name}': the joint check stopped: {e}"))?;
    let (mut broken, mut unproved) = (Vec::new(), Vec::new());
    for result in &invariants {
        match result.outcome {
            JointOutcome::Holds => tracing::info!(tenant, app = %app_name, "{result}"),
            JointOutcome::Incomplete { .. } => unproved.push(result.to_string()),
            JointOutcome::Broken { .. } => broken.push(result.to_string()),
        }
    }
    for result in &liveness {
        match result.outcome {
            LivenessOutcome::Holds => tracing::info!(tenant, app = %app_name, "{result}"),
            LivenessOutcome::Incomplete { .. } => unproved.push(result.to_string()),
            LivenessOutcome::Stuck { .. } => broken.push(result.to_string()),
        }
    }
    for text in &unproved {
        tracing::warn!(tenant, app = %app_name, "{text}");
    }
    if broken.is_empty() {
        Ok(unproved)
    } else {
        Err(format!(
            "os-app '{app_name}' would break tenant rules over several records:\n{}",
            broken.join("\n")
        ))
    }
}

/// The bundle's types parsed, with each type `names` returns for them taken
/// from what the tenant already runs when the bundle does not carry it.
/// Include active tenant types before finding properties;
/// incoming types take precedence. `None` when no checked type names one.
fn bundle_with_named(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    names: impl Fn(&temper_spec::automaton::Automaton) -> Vec<(String, String)>,
) -> Result<Option<std::collections::BTreeMap<String, temper_spec::automaton::Automaton>>, String> {
    let mut automatons = std::collections::BTreeMap::new();
    for (entity_type, ioa_source) in specs {
        let automaton = temper_spec::automaton::parse_automaton(ioa_source)
            .map_err(|e| format!("os-app '{app_name}' spec {entity_type} failed to parse: {e}"))?;
        automatons.insert(entity_type.clone(), automaton);
    }
    let tenant_id = temper_runtime::tenant::TenantId::new(tenant);
    let registry = state
        .registry
        .read()
        .map_err(|_| format!("os-app '{app_name}': the spec registry lock is poisoned"))?;
    // Include installed rule owners and all their supporting types.
    for entity_type in registry.entity_types(&tenant_id) {
        if let Some(spec) = registry.get_spec(&tenant_id, entity_type) {
            automatons
                .entry(entity_type.to_string())
                .or_insert_with(|| spec.automaton.clone());
        }
    }
    let named: std::collections::BTreeSet<String> = automatons
        .values()
        .flat_map(&names)
        .map(|(entity_type, _)| entity_type)
        .collect();
    if named.is_empty() {
        return Ok(None);
    }
    Ok(Some(automatons))
}

/// Check each new version in `specs`, when the install writes specs, against
/// the records stored under the version installed for its type, before
/// anything is written (see temper-server's `spec_change`): refuse it when it
/// leaves records with no way forward that no `[[carry]]` moves. Returns what
/// the check reports, and how many records each carry moves once the install
/// has answered (the server's carry pass makes them in the background).
pub(super) async fn check_spec_changes(
    state: &PlatformState,
    tenant: &TenantId,
    app_name: &str,
    specs: &[(String, String)],
    writes_specs: bool,
) -> Result<Vec<InstallWarning>, String> {
    if !writes_specs {
        return Ok(Vec::new());
    }
    let sources: Vec<(&str, &str)> = specs
        .iter()
        .map(|(entity_type, source)| (entity_type.as_str(), source.as_str()))
        .collect();
    let changes = state
        .server
        .spec_changes(tenant, &sources)
        .map_err(|error| format!("os-app '{app_name}': {error}"))?;
    let plan = state
        .server
        .check_spec_changes(tenant, &changes)
        .await
        .map_err(|refused| format!("os-app '{app_name}' was not installed. {refused}"))?;
    let mut carrying: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for carry in &plan.carries {
        *carrying
            .entry((carry.entity_type.as_str(), carry.action.as_str()))
            .or_default() += 1;
    }
    let carrying = carrying
        .into_iter()
        .map(|((entity_type, action), records)| {
            let records = if records == 1 {
                "1 record".to_string()
            } else {
                format!("{records} records")
            };
            format!(
                "{entity_type}: {action} carries {records} once the install has answered; \
             until then they wait in their old state"
            )
        });
    Ok(plan
        .warnings
        .into_iter()
        .chain(carrying)
        .map(InstallWarning::spec_change)
        .collect())
}

#[cfg(test)]
#[path = "install_checks_test.rs"]
mod tests;
