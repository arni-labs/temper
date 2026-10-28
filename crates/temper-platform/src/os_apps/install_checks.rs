//! Checks an os-app install makes before it writes anything.
//!
//! A bundle that cannot be registered must leave no stored row for the next
//! start to restore, so these refuse it before the first write.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use temper_authz::{Asker, May, SendAnalysis};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::Automaton;
use temper_spec::cross_invariant;
use temper_verify::composite::{Bound, JointOutcome, LivenessOutcome};

use super::InstallWarning;
use super::proofs::{self, Attempt, Proof, Verdict, Wait, Walked};

use crate::state::PlatformState;

/// Types by name, each with its spec source and parsed.
type Sourced = BTreeMap<String, (String, Automaton)>;

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
/// leaves it) deciding each move as its sender. Both run when the install
/// brings `new_specs` (added or changed types) or policies: who Cedar lets
/// send each move decides whether a rule holds and whether a record gets
/// where it should. A named type the bundle does not carry is taken from what
/// the tenant already runs. Policy and spec changes check every active tenant
/// spec, overlaid with the incoming specs.
///
/// Only a rule the check finds broken refuses the install: an invariant with
/// a path of moves that breaks it, or a liveness rule with a path to a state
/// from which no move reaches its condition. A rule it could not prove (the
/// walk did not fit in its memory, or it reads a type nobody declares) is a
/// warning and the install goes ahead; those rules are returned for the
/// install's answer. It runs before the install writes or swaps anything, so
/// a refused bundle leaves the tenant as it was and reads keep serving the
/// old specs meanwhile. The walk runs as its own job (see [`proofs`]): a
/// proof still walking comes back as the walk to wait for, and the install
/// waits for it without its lock and starts again, finding the verdict in
/// `attempt`.
pub(super) async fn prove(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    new_specs: bool,
    policies: Option<&str>,
    attempt: &Attempt,
) -> Result<ControlFlow<Vec<String>, Wait>, String> {
    if !new_specs && policies.is_none() {
        return Ok(ControlFlow::Break(Vec::new()));
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
        return Ok(ControlFlow::Break(Vec::new()));
    };
    let policy_text = policies.map_or_else(
        || super::cached_or_active_tenant_policy_text(state, tenant),
        str::to_string,
    );
    let policies = SendAnalysis::new(&policy_text)
        .map_err(|e| format!("os-app '{app_name}': its Cedar could not be read: {e}"))?;
    const PROOF: &str = "rules over several records";
    let digest = proofs::digest(PROOF, &sources(&automatons), Some(&policy_text));
    let app = app_name.to_string();
    let walk = move |bound: Bound<'_>| {
        let refs: Vec<&Automaton> = automatons.values().map(|(_, a)| a).collect();
        let invariants =
            temper_verify::composite::verify_joint_invariants(&refs, Some(&policies), bound);
        let liveness =
            temper_verify::composite::verify_joint_liveness(&refs, Some(&policies), bound);
        let (mut held, mut broken, mut unproved) = (Vec::new(), Vec::new(), Vec::new());
        for result in &invariants {
            match result.outcome {
                JointOutcome::Holds => held.push(result.to_string()),
                JointOutcome::Incomplete { .. } => unproved.push(result.to_string()),
                JointOutcome::Broken { .. } => broken.push(result.to_string()),
            }
        }
        for result in &liveness {
            match result.outcome {
                LivenessOutcome::Holds => held.push(result.to_string()),
                LivenessOutcome::Incomplete { .. } => unproved.push(result.to_string()),
                LivenessOutcome::Stuck { .. } => broken.push(result.to_string()),
            }
        }
        // A rule left unproved says nothing of the specs: its walk may fit
        // when the host has more room, so its verdict is not kept.
        let fit = unproved.is_empty();
        let refused = (!broken.is_empty()).then(|| {
            format!(
                "os-app '{app}' would break tenant rules over several records:\n{}",
                broken.join("\n")
            )
        });
        Walked {
            verdict: Verdict {
                held,
                unproved,
                refused,
            },
            fit,
        }
    };
    let proof = proofs::proof(state, tenant, app_name, PROOF, digest, attempt, walk).await;
    decided(tenant, app_name, proof)
}

/// The install goes on with the rules left unproved, is refused, or waits
/// for the walk.
fn decided(
    tenant: &str,
    app_name: &str,
    proof: Proof,
) -> Result<ControlFlow<Vec<String>, Wait>, String> {
    let verdict = match proof {
        Proof::Running(wait) => return Ok(ControlFlow::Continue(wait)),
        Proof::Done(verdict) => verdict,
    };
    if let Some(refused) = verdict.refused {
        return Err(refused);
    }
    for line in &verdict.held {
        tracing::info!(tenant, app = %app_name, "{line}");
    }
    for line in &verdict.unproved {
        tracing::warn!(tenant, app = %app_name, "{line}");
    }
    Ok(ControlFlow::Break(verdict.unproved))
}

/// [`prove`] as an install drives it: wait for the walk while it runs, then
/// ask again, until it answers.
#[cfg(test)]
pub(super) async fn prove_to_the_end(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    new_specs: bool,
    policies: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut attempt = Attempt::default();
    loop {
        let proved = prove(
            state, tenant, app_name, specs, new_specs, policies, &attempt,
        );
        match proved.await? {
            ControlFlow::Continue(wait) => wait.ended(&mut attempt).await?,
            ControlFlow::Break(unproved) => return Ok(unproved),
        }
    }
}

/// Each type's spec source, which a verdict is kept by.
fn sources(automatons: &Sourced) -> BTreeMap<String, String> {
    automatons
        .iter()
        .map(|(entity_type, (source, _))| (entity_type.clone(), source.clone()))
        .collect()
}

/// The bundle's types, each with its source and parsed, with every type the
/// tenant already runs that the bundle does not carry: incoming types take
/// precedence. `None` when no type `names` returns one.
fn bundle_with_named(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    names: impl Fn(&Automaton) -> Vec<(String, String)>,
) -> Result<Option<Sourced>, String> {
    let mut automatons = BTreeMap::new();
    for (entity_type, ioa_source) in specs {
        let automaton = temper_spec::automaton::parse_automaton(ioa_source)
            .map_err(|e| format!("os-app '{app_name}' spec {entity_type} failed to parse: {e}"))?;
        automatons.insert(entity_type.clone(), (ioa_source.clone(), automaton));
    }
    let tenant_id = TenantId::new(tenant);
    let registry = state
        .registry
        .read()
        .map_err(|_| format!("os-app '{app_name}': the spec registry lock is poisoned"))?;
    // Include installed rule owners and all their supporting types.
    for entity_type in registry.entity_types(&tenant_id) {
        if let Some(spec) = registry.get_spec(&tenant_id, entity_type) {
            automatons
                .entry(entity_type.to_string())
                .or_insert_with(|| (spec.ioa_source.clone(), spec.automaton.clone()));
        }
    }
    let named: std::collections::BTreeSet<String> = automatons
        .values()
        .flat_map(|(_, a)| names(a))
        .map(|(entity_type, _)| entity_type)
        .collect();
    if named.is_empty() {
        return Ok(None);
    }
    Ok(Some(automatons))
}

/// What the joint model says of `specs` under `policies` without refusing the
/// install: the rules [`prove`] could not prove, and the timers whose lost
/// fire nothing redoes ([`timer_strands`]). Or the proof's walk to wait for,
/// as [`prove`] gives it.
pub(super) async fn model_warnings(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    specs: &[(String, String)],
    new_specs: bool,
    policies: Option<&str>,
    attempt: &Attempt,
) -> Result<ControlFlow<Vec<InstallWarning>, Wait>, String> {
    let proved = prove(state, tenant, app_name, specs, new_specs, policies, attempt);
    let unproved = match proved.await? {
        ControlFlow::Continue(wait) => return Ok(ControlFlow::Continue(wait)),
        ControlFlow::Break(unproved) => unproved,
    };
    let mut warnings: Vec<InstallWarning> = unproved
        .into_iter()
        .map(InstallWarning::joint_invariants)
        .collect();
    warnings.extend(timer_strands(state, tenant, specs, new_specs, policies));
    Ok(ControlFlow::Break(warnings))
}

/// Warn about each state timer of `specs` whose lost fire nothing redoes: no
/// one outside Temper may make its `on_timeout` move, or any move in its
/// `reset_on`, under `policies` (the tenant's Cedar as the install leaves
/// it). A fire is lost to a projection timeout, a restart before the boot
/// re-arm, or a refused fire, and the record then stays in the timer's state.
/// A move is one someone outside may make unless Cedar refuses it whatever
/// the request holds (`May::No`), as the joint model decides an outside move,
/// so each warning is certain. With no policies everyone may make every move,
/// and nothing warns. It runs when the install brings new specs or policies,
/// like [`prove`], and never refuses: a rule that cannot block yet starts as
/// a warning.
pub(super) fn timer_strands(
    state: &PlatformState,
    tenant: &str,
    specs: &[(String, String)],
    new_specs: bool,
    policies: Option<&str>,
) -> Vec<InstallWarning> {
    if !new_specs && policies.is_none() {
        return Vec::new();
    }
    let policy_text = policies.map_or_else(
        || super::cached_or_active_tenant_policy_text(state, tenant),
        str::to_string,
    );
    // Cedar that does not compile was refused before this (`install_guard`).
    let Some(cedar) = Some(policy_text)
        .filter(|text| !text.trim().is_empty())
        .and_then(|text| SendAnalysis::new(&text).ok())
    else {
        return Vec::new();
    };
    let outside_may = |entity_type: &str, action: &str| {
        cedar.may(&Asker::Outside, action, entity_type, &BTreeMap::new()) != May::No
    };
    let mut warnings = Vec::new();
    for (entity_type, source) in specs {
        // A spec that does not parse is refused by the install's other checks.
        let Ok(automaton) = temper_spec::automaton::parse_automaton(source) else {
            continue;
        };
        for timer in &automaton.state_timeouts {
            let redone = std::iter::once(&timer.on_timeout)
                .chain(&timer.reset_on)
                .any(|action| outside_may(entity_type, action));
            if redone {
                continue;
            }
            let (place, action, seconds) = (&timer.state, &timer.on_timeout, timer.after_seconds);
            warnings.push(InstallWarning::timer_strands(
                format!(
                    "{entity_type}: {place}'s timer makes {action} after {seconds} s, and no \
                     person or agent outside Temper may make {action} or a move in its \
                     reset_on, so a record whose fire is lost (a projection timeout, a restart \
                     before the boot re-arm, a refused fire) stays in {place}. Let a person \
                     make {action}, so the same move redoes a lost fire, or list a move a \
                     person may make in reset_on."
                ),
                serde_json::json!({
                    "entity_type": entity_type,
                    "state": place,
                    "on_timeout": action,
                    "after_seconds": seconds,
                }),
            ));
        }
    }
    warnings
}

/// Check each new version in `specs`, when the install writes specs, against
/// the records stored under the version installed for its type, before
/// anything is written (see temper-server's `spec_change`): refuse it when it
/// leaves records with no way forward that no `[[carry]]` moves. Returns what
/// the check reports, and how many records each carry moves once the install
/// has answered (the server's carry pass makes them in the background).
/// `policies` is the tenant's Cedar text once the install serves, when the
/// install changes it.
pub(super) async fn check_spec_changes(
    state: &PlatformState,
    tenant: &TenantId,
    app_name: &str,
    specs: &[(String, String)],
    writes_specs: bool,
    policies: Option<&str>,
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
        .check_spec_changes(tenant, &changes, policies)
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
