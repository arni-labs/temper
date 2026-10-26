//! Checks an os-app install makes before it writes anything.
//!
//! A bundle that cannot be registered must leave no stored row for the next
//! start to restore, so these refuse it before the first write.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use opentelemetry::metrics::Counter;
use opentelemetry::{KeyValue, global};
use temper_spec::automaton::Automaton;
use temper_spec::cross_invariant;
use temper_verify::composite::{Bound, JointProof, PROOF_TIME};

use super::proofs::{self, Keys, Proof, Verdict, Wait, Walked};
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

/// Prove every `[[invariant]]` and every `[[liveness]]` with `eventually`
/// of `specs` (a bundle's types), on the joint model of its type and the
/// types it names, with `policies` (the tenant's Cedar as the install leaves
/// it) deciding each move as its sender, before the install writes anything.
/// Each rule is proved by induction first, for every run (an invariant with
/// its lemmas or the prover's guesses, an `eventually` rule with its
/// measure), and walked only when the proof does not hold
/// ([`JointProof::check`]). Both run when the install brings `new_specs`
/// (added or changed types) or policies: who Cedar lets send each move
/// decides whether a rule holds and whether a record gets where it should. A
/// named type the bundle does not carry is taken from what the tenant
/// already runs. Each rule's verdict is kept by the digest of its walk's
/// model and by the digest of the specs and Cedar (see [`proofs`]), so a
/// rule whose cut model the install leaves as it was is not proved again,
/// nor one whose specs a new kernel starts with. A proof still running
/// comes back as the proof to wait for: the install waits for it without
/// its lock and starts again.
///
/// A rule neither proves nor walks refuses the install, unless the tenant
/// runs it now as it is: its digest is the one the tenant's specs and Cedar
/// give it ([`running_digests`]). A better checker then must not take a
/// running app down at its next start; the rule is logged and counted, it
/// keeps running, and the next change to it is refused until it is proved.
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
    // A rule's own type counts as named, so one that reads no other type is
    // still checked.
    let Some(automatons) = bundle_with_named(state, tenant, app_name, specs, |a| {
        let invariants = a.invariants.iter().map(|inv| &inv.assert);
        let liveness = a
            .liveness
            .iter()
            .filter_map(|live| live.eventually.as_ref());
        let own = (a.automaton.name.clone(), String::new());
        invariants
            .chain(liveness)
            .flat_map(|rule| rule.related_types().into_iter().chain([own.clone()]))
            .collect()
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
    let sources = sources(&automatons);
    let types: Vec<String> = automatons.keys().cloned().collect();
    let joint = proofs_of(app_name, automatons.into_values().map(|(_, a)| a), policies).await?;
    let mut held = Vec::new();
    let mut refused = Vec::new();
    let mut running = None;
    for joint_proof in joint {
        let name = joint_proof.name();
        let keys = Keys {
            model: joint_proof.digest(),
            source: proofs::digest(&name, &sources, Some(&policy_text)),
        };
        let digest = keys.model.clone();
        let proof = proofs::proof(state, tenant, app_name, name.clone(), keys, move |bound| {
            walk(&joint_proof, bound)
        })
        .await;
        match proof {
            Proof::Done(verdict) => {
                held.extend(verdict.held);
                refused.extend(verdict.refused.map(|why| (name, digest, why)));
            }
            Proof::Running(wait) => {
                running.get_or_insert(wait);
            }
        }
    }
    let mut unproven = Vec::new();
    if !refused.is_empty() {
        let runs_now = running_digests(state, tenant, app_name, &types).await?;
        for (rule, digest, why) in refused {
            if runs_now.get(&rule) != Some(&digest) {
                unproven.push(why);
                continue;
            }
            tracing::warn!(
                tenant,
                app = %app_name,
                %rule,
                "a rule the tenant runs as it is is not proven; it keeps running, and a \
                 change to it is refused until it is: {why}"
            );
            record_running_unproven(tenant, app_name, &rule);
        }
    }
    if !unproven.is_empty() {
        return Err(format!(
            "os-app '{app_name}' declares rules that are not proven:\n{}",
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

/// The proof of each rule of `automatons`, with `policies` deciding each
/// move as its sender. Composing a model asks Cedar about each sender and
/// action in its scope, so it runs off the async threads.
async fn proofs_of(
    app_name: &str,
    automatons: impl Iterator<Item = Automaton>,
    policies: temper_authz::SendAnalysis,
) -> Result<Vec<JointProof>, String> {
    let automatons: Vec<Automaton> = automatons.collect();
    tokio::task::spawn_blocking(move || {
        let refs: Vec<&Automaton> = automatons.iter().collect();
        temper_verify::composite::joint_proofs(&refs, Some(&policies))
    })
    .await
    .map_err(|e| format!("os-app '{app_name}': its rules could not be read: {e}"))
}

/// The digest of each rule's proof, by its name, as the tenant runs it now:
/// with the specs it runs for `types` and its live Cedar. A type it does not
/// run yet has no rules here.
async fn running_digests(
    state: &PlatformState,
    tenant: &str,
    app_name: &str,
    types: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let running: Vec<Automaton> = {
        let tenant_id = temper_runtime::tenant::TenantId::new(tenant);
        let registry = state
            .registry
            .read()
            .map_err(|_| format!("os-app '{app_name}': the spec registry lock is poisoned"))?;
        types
            .iter()
            .filter_map(|entity_type| registry.get_spec(&tenant_id, entity_type))
            .map(|spec| spec.automaton.clone())
            .collect()
    };
    let live = super::cached_or_active_tenant_policy_text(state, tenant);
    let policies = temper_authz::SendAnalysis::new(&live)
        .map_err(|e| format!("os-app '{app_name}': the tenant's Cedar could not be read: {e}"))?;
    let proofs = proofs_of(app_name, running.into_iter(), policies).await?;
    Ok(proofs.iter().map(|p| (p.name(), p.digest())).collect())
}

/// Count a rule an install let run unproved, since the tenant already runs
/// it as it is.
fn record_running_unproven(tenant: &str, app_name: &str, rule: &str) {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    let counter = COUNTER.get_or_init(|| {
        global::meter("temper.platform")
            .u64_counter("temper_os_app_unproven_rule_running_total")
            .with_description(
                "Rules an os-app install found unproven and let run, since the tenant already \
                 runs them as they are.",
            )
            .build()
    });
    counter.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("app", app_name.to_string()),
            KeyValue::new("rule", rule.to_string()),
        ],
    );
}

/// Prove one rule, by induction and then by its walk when the proof does
/// not hold: its verdict, and whether the walk fit in its memory. One that
/// did not says nothing of the specs, and may fit when the host has more
/// room.
fn walk(proof: &JointProof, bound: Bound<'_>) -> Walked {
    let verdict = proof.check(bound, PROOF_TIME);
    let text = verdict.to_string();
    let (held, refused) = match verdict.holds() {
        true => (vec![text], None),
        false => (Vec::new(), Some(text)),
    };
    Walked {
        verdict: Verdict { held, refused },
        fit: verdict.tag() != "INCOMPLETE",
    }
}

/// Each type's spec source, which a verdict is also kept by.
fn sources(automatons: &Sourced) -> BTreeMap<String, String> {
    automatons
        .iter()
        .map(|(entity_type, (source, _))| (entity_type.clone(), source.clone()))
        .collect()
}

/// The bundle's types, each with its source and parsed, with each type
/// `names` returns for them taken from what the tenant already runs when the
/// bundle does not carry it. `None` when no bundle type names one.
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
    let named: std::collections::BTreeSet<String> = automatons
        .values()
        .flat_map(|(_, a)| names(a))
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
            automatons.insert(
                entity_type.clone(),
                (spec.ioa_source.clone(), spec.automaton.clone()),
            );
        }
    }
    Ok(Some(automatons))
}
