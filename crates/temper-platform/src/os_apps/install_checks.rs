//! Checks an os-app install makes before it writes anything.
//!
//! A bundle that cannot be registered must leave no stored row for the next
//! start to restore, so these refuse it before the first write.

use std::collections::BTreeMap;

use temper_spec::automaton::Automaton;
use temper_spec::cross_invariant;
use temper_verify::composite::{Bound, JointOutcome, JointProof, LivenessOutcome};

use super::proofs::{self, Proof, Verdict, Wait, Walked};
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

/// Prove every `[[invariant]]` and `[[liveness]]` of `specs` (a bundle's
/// types) that reads a related record, on the joint model of its type and
/// the types it names, with `policies` (the tenant's Cedar as the install
/// leaves it) deciding each move as its sender, before the install writes
/// anything. Both run when the install brings `new_specs` (added or changed
/// types) or policies: who Cedar lets send each move decides whether a rule
/// holds and whether a record gets where it should. A named type the bundle
/// does not carry is taken from what the tenant already runs. Each walk's
/// verdict is kept by the digest of its model (see [`proofs`]), so a rule
/// whose cut model the install leaves as it was is not walked again. A
/// walk still running comes back as the walk to wait for: the install waits
/// for it without its lock and starts again.
pub(super) async fn prove(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    new_specs: bool,
    policies: Option<&str>,
) -> Result<Option<Wait>, String> {
    if !new_specs && policies.is_none() {
        return Ok(None);
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
        return Ok(None);
    };
    let policy_text = policies.map_or_else(
        || super::cached_or_active_tenant_policy_text(state, tenant),
        str::to_string,
    );
    let policies = temper_authz::SendAnalysis::new(&policy_text)
        .map_err(|e| format!("os-app '{app_name}': its Cedar could not be read: {e}"))?;
    // Composing a model asks Cedar about each sender and action in its
    // scope, so it runs off the async threads too.
    let joint = tokio::task::spawn_blocking(move || {
        let refs: Vec<&Automaton> = automatons.values().collect();
        temper_verify::composite::joint_proofs(&refs, Some(&policies))
    })
    .await
    .map_err(|e| {
        format!("os-app '{app_name}': its rules over several records could not be read: {e}")
    })?;
    let mut held = Vec::new();
    let mut unproven = Vec::new();
    let mut running = None;
    for joint_proof in joint {
        let name = joint_proof.name();
        let digest = joint_proof.digest();
        let proof = proofs::proof(state, tenant, app_name, name, digest, move |bound| {
            walk(&joint_proof, bound)
        })
        .await;
        match proof {
            Proof::Done(verdict) => {
                held.extend(verdict.held);
                unproven.extend(verdict.refused);
            }
            Proof::Running(wait) => {
                running.get_or_insert(wait);
            }
        }
    }
    if !unproven.is_empty() {
        return Err(format!(
            "os-app '{app_name}' declares rules over several records that are not proven:\n{}",
            unproven.join("\n")
        ));
    }
    if running.is_some() {
        return Ok(running);
    }
    for line in &held {
        tracing::info!(tenant, app = %app_name, "{line}");
    }
    Ok(None)
}

/// Walk one proof: the results that held, those that did not, and whether
/// the walk fit in its memory. One that did not says nothing of the specs,
/// and may fit when the host has more room.
fn walk(proof: &JointProof, bound: Bound<'_>) -> Walked {
    let results: Vec<(bool, bool, String)> = match proof {
        JointProof::Invariants(invariants) => invariants
            .check(bound)
            .iter()
            .map(|r| {
                let fit = !matches!(r.outcome, JointOutcome::Incomplete { .. });
                (r.holds(), fit, r.to_string())
            })
            .collect(),
        JointProof::Liveness(liveness) => {
            let r = liveness.check(bound);
            let fit = !matches!(r.outcome, LivenessOutcome::Incomplete { .. });
            vec![(r.holds(), fit, r.to_string())]
        }
    };
    let fit = results.iter().all(|(_, fit, _)| *fit);
    let (held, unproven): (Vec<_>, Vec<_>) = results.into_iter().partition(|(holds, ..)| *holds);
    let unproven: Vec<String> = unproven.into_iter().map(|(.., text)| text).collect();
    Walked {
        verdict: Verdict {
            held: held.into_iter().map(|(.., text)| text).collect(),
            refused: (!unproven.is_empty()).then(|| unproven.join("\n")),
        },
        fit,
    }
}

/// The bundle's types, parsed, with each type `names` returns for them taken
/// from what the tenant already runs when the bundle does not carry it.
/// `None` when no bundle type names one.
fn bundle_with_named(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    names: impl Fn(&Automaton) -> Vec<(String, String)>,
) -> Result<Option<BTreeMap<String, Automaton>>, String> {
    let mut automatons = BTreeMap::new();
    for (entity_type, ioa_source) in specs {
        let automaton = temper_spec::automaton::parse_automaton(ioa_source)
            .map_err(|e| format!("os-app '{app_name}' spec {entity_type} failed to parse: {e}"))?;
        automatons.insert(entity_type.clone(), automaton);
    }
    let named: std::collections::BTreeSet<String> = automatons
        .values()
        .flat_map(&names)
        .map(|(entity_type, _)| entity_type)
        .collect();
    if named.is_empty() {
        return Ok(None);
    }
    let tenant_id = temper_runtime::tenant::TenantId::new(tenant);
    let registry = state
        .registry
        .read()
        .map_err(|_| format!("os-app '{app_name}': the spec registry lock is poisoned"))?;
    for entity_type in &named {
        if !automatons.contains_key(entity_type)
            && let Some(spec) = registry.get_spec(&tenant_id, entity_type)
        {
            automatons.insert(entity_type.clone(), spec.automaton.clone());
        }
    }
    Ok(Some(automatons))
}
