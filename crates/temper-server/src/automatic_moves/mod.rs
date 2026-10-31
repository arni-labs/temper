//! Whether an app's automatic moves can be permitted as the principals that
//! will send them.
//!
//! Reactions, module and adapter answers, state timers, scheduled actions and
//! spawned children each take their principal from one function in the
//! dispatcher. This check calls those functions with a representative of each
//! identity kind and asks Cedar about the result. A second copy of the
//! principal rule is how a callback's follow-up was still treated as
//! `service:wasm-runtime` after the dispatcher stopped sending it that way
//! (temper#194, Far Bank `a7lo6uzfupl4`).
//!
//! FOLLOW-UP(kernel-upgrade-recheck): not wired. See ADR-0184. On install and
//! on a spec or Cedar change, a refusal blocks the install. Apps already
//! installed when the kernel binary changes are not rechecked here. That pass
//! must report the affected apps and keep serving them; refusing to boot
//! would take the apps down.
//!
//! A state timer whose arming principal was not stored fires as
//! `service:state-timer`. This check does not treat that boot fallback as a
//! sender. A permit that names only `service:state-timer` does not cover the
//! principal who armed the timer.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use temper_authz::{
    Asker, Attr, AuthzEngine, May, MoveCause, SecurityContext, SendAnalysis, cause_context,
    platform_keys,
};
use temper_spec::automaton::{
    Action, ActionKind, Automaton, LivenessEnforcement, parse_automaton_with_liveness,
};

use crate::request_context::AgentContext;

mod walk;

/// Cedar refuses this follow-up for the principal the dispatcher will use.
pub const REFUSED_FOLLOW_UP: &str = "refused_follow_up";
/// A permit names a service principal that sends no move of these specs.
pub const DEAD_SERVICE_PERMIT: &str = "dead_service_permit";
/// Cedar might allow the follow-up, depending on the record or which principal
/// of that kind sends it. Not a refusal.
pub const CONDITIONAL_FOLLOW_UP: &str = "conditional_follow_up";

/// Marker for the boot-time recheck. It is not wired.
pub const KERNEL_UPGRADE_RECHECK: &str =
    "not wired: report affected apps when the kernel changes; do not refuse to boot";

/// The id used where the dispatcher wants a record id. Policies that match a
/// particular id come out conditional, because this id is not a real record.
const RECORD: &str = "record";

/// What [`check`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Install must stop. The move cannot be permitted, or a permit names a
    /// service that never sends a move.
    pub errors: Vec<Finding>,
    /// The move might be permitted for some records or some principals of
    /// that kind. Install may proceed.
    pub warnings: Vec<Finding>,
}

/// One automatic move, or one unused service permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// [`REFUSED_FOLLOW_UP`], [`DEAD_SERVICE_PERMIT`] or [`CONDITIONAL_FOLLOW_UP`].
    pub code: &'static str,
    /// The record type the move acts on. Empty for a dead service permit.
    pub entity_type: String,
    /// The move. Empty for a dead service permit.
    pub action: String,
    /// `outside` or `service:<name>`.
    pub principal: String,
    /// `Type.Action` of the move that set this one off, when it is a follow-up.
    pub after: Option<String>,
    /// What happened, in words.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Outside,
    Service(String),
}

struct Pending {
    entity: String,
    action: String,
    kind: Kind,
    cause: Option<MoveCause>,
}

type Seen = BTreeSet<(String, String, Kind, &'static str)>;

/// Check `specs` (`(entity type, IOA source)`, the automaton's own name is the
/// type) under `policy` (the tenant's Cedar as the install would leave it).
///
/// An empty policy permits every move: there is no Cedar to refuse one. A
/// policy that does not parse is an error. A move Cedar refuses for the
/// derived principal is [`Report::errors`]. A move Cedar might allow once the
/// record's attributes or the particular outside principal are known is
/// [`Report::warnings`].
///
/// Cedar is asked about entity triggers, which is where the dispatcher asks.
/// A module answer, a state timer's own fire and a scheduled action are not
/// action-authorized; they carry their principal into the triggers that follow.
pub fn check(specs: &[(&str, &str)], policy: &str) -> Result<Report, String> {
    if policy.trim().is_empty() {
        return Ok(Report {
            errors: Vec::new(),
            warnings: Vec::new(),
        });
    }
    let analysis =
        SendAnalysis::new(policy).map_err(|error| format!("Cedar could not be read: {error}"))?;
    let engine =
        AuthzEngine::new(policy).map_err(|error| format!("Cedar could not be read: {error}"))?;
    let by_name = parse_specs(specs)?;

    let mut seen: Seen = BTreeSet::new();
    let mut work: Vec<Pending> = Vec::new();
    for (entity, automaton) in &by_name {
        for action in &automaton.actions {
            if !matches!(action.kind, ActionKind::Input | ActionKind::Composite) {
                continue;
            }
            if analysis.may(&Asker::Outside, &action.name, entity, &BTreeMap::new()) != May::No {
                remember(
                    &mut seen,
                    &mut work,
                    entity,
                    &action.name,
                    &context_for(&Kind::Outside, None),
                );
            }
        }
    }

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut next = 0;
    while next < work.len() {
        let pending = &work[next];
        let entity = pending.entity.clone();
        let action_name = pending.action.clone();
        let kind = pending.kind.clone();
        let cause = pending.cause.clone();
        next += 1;
        let Some(action) = find_action(&by_name, &entity, &action_name) else {
            continue;
        };
        let invoker = context_for(&kind, cause);
        walk::expand_triggers(
            &analysis,
            &engine,
            &entity,
            &action_name,
            action,
            &invoker,
            &mut seen,
            &mut work,
            &mut errors,
            &mut warnings,
        );
        walk::expand_effects(&entity, action, &invoker, &mut seen, &mut work);
        if let Some(automaton) = by_name.get(&entity) {
            walk::expand_timers(automaton, action, &invoker, &mut seen, &mut work);
        }
    }

    let senders = seen
        .iter()
        .filter_map(|(_, _, kind, _)| match kind {
            Kind::Service(name) => Some(name.clone()),
            Kind::Outside => None,
        })
        .collect::<BTreeSet<_>>();
    for name in analysis.permit_services() {
        if senders.contains(&name) {
            continue;
        }
        errors.push(Finding {
            code: DEAD_SERVICE_PERMIT,
            entity_type: String::new(),
            action: String::new(),
            principal: format!("service:{name}"),
            after: None,
            detail: format!(
                "A permit names service:{name}, which never sends a move of these specs. \
                 Automatic moves run as the principal the dispatcher derives — the caller, \
                 or a trigger's declared principal — so this permit authorizes no move."
            ),
        });
    }

    Ok(Report {
        errors: dedup(errors),
        warnings: dedup(warnings),
    })
}

fn parse_specs(specs: &[(&str, &str)]) -> Result<BTreeMap<String, Automaton>, String> {
    let mut by_name = BTreeMap::new();
    for (label, source) in specs {
        let automaton = parse_automaton_with_liveness(source, LivenessEnforcement::WarnOnly)
            .map_err(|error| format!("spec '{label}' failed to parse: {error}"))?;
        let name = automaton.automaton.name.clone();
        if !label.is_empty() && *label != name {
            return Err(format!("spec '{label}' declares automaton '{name}'"));
        }
        by_name.insert(name, automaton);
    }
    Ok(by_name)
}

fn find_action<'a>(
    by_name: &'a BTreeMap<String, Automaton>,
    entity: &str,
    action: &str,
) -> Option<&'a Action> {
    by_name
        .get(entity)?
        .actions
        .iter()
        .find(|candidate| candidate.name == action)
}

fn judge(
    analysis: &SendAnalysis,
    engine: &AuthzEngine,
    ctx: &AgentContext,
    security: &SecurityContext,
    source_entity: &str,
    source_action: &str,
    target_entity: &str,
    target_action: &str,
    errors: &mut Vec<Finding>,
    warnings: &mut Vec<Finding>,
) {
    let kind = kind_of(ctx);
    let asker = asker_for(security);
    let after = format!("{source_entity}.{source_action}");
    let principal = principal_id(&kind);
    match analysis.may(&asker, target_action, target_entity, &BTreeMap::new()) {
        May::No => errors.push(refused(
            target_entity,
            target_action,
            &principal,
            &after,
            &format!(
                "{target_entity}.{target_action} following {after} runs as {principal}, \
                 and Cedar refuses that principal."
            ),
        )),
        May::Yes => {}
        May::Maybe => {
            if !allows(engine, security, target_action, target_entity) {
                warnings.push(Finding {
                    code: CONDITIONAL_FOLLOW_UP,
                    entity_type: target_entity.to_string(),
                    action: target_action.to_string(),
                    principal: principal.clone(),
                    after: Some(after.clone()),
                    detail: format!(
                        "{target_entity}.{target_action} following {after} runs as {principal}. \
                         Cedar does not allow it for a representative of that principal with an \
                         empty record. It depends on the record's attributes, or on which \
                         principal of that kind sends it."
                    ),
                });
            }
        }
    }
}

fn refused(entity: &str, action: &str, principal: &str, after: &str, detail: &str) -> Finding {
    Finding {
        code: REFUSED_FOLLOW_UP,
        entity_type: entity.to_string(),
        action: action.to_string(),
        principal: principal.to_string(),
        after: Some(after.to_string()),
        detail: detail.to_string(),
    }
}

fn allows(engine: &AuthzEngine, security: &SecurityContext, action: &str, entity: &str) -> bool {
    // determinism-ok: Cedar's authorize takes a HashMap; this check does not iterate it
    let mut attrs = std::collections::HashMap::new();
    attrs.insert(
        "id".to_string(),
        serde_json::Value::String(RECORD.to_string()),
    );
    engine
        .authorize(security, action, entity, &attrs)
        .is_allowed()
}

fn asker_for(security: &SecurityContext) -> Asker {
    let Some(name) = security
        .principal
        .id
        .strip_prefix("service:")
        .filter(|name| !name.is_empty())
    else {
        return Asker::Outside;
    };
    let mut context = BTreeMap::new();
    for (key, value) in &security.context_attrs {
        if !platform_keys::CAUSE.contains(&key.as_str()) {
            context.insert(key.clone(), Attr::Value(value.clone()));
        }
    }
    for (key, value) in cause_context(security.cause.as_ref()) {
        context.insert(key.to_string(), Attr::Value(value));
    }
    Asker::Service {
        name: name.to_string(),
        context,
    }
}

fn context_for(kind: &Kind, cause: Option<MoveCause>) -> AgentContext {
    match kind {
        Kind::Outside => {
            let mut security = SecurityContext::anonymous();
            security.principal.id = "member".to_string();
            security.cause = cause;
            AgentContext {
                security_ctx: Some(security),
                agent_id: Some("member".to_string()),
                ..AgentContext::default()
            }
        }
        Kind::Service(name) => {
            let mut ctx = AgentContext::for_service(name);
            if let Some(security) = ctx.security_ctx.as_mut() {
                security.cause = cause;
            }
            ctx
        }
    }
}

fn kind_of(ctx: &AgentContext) -> Kind {
    let id = ctx
        .security_ctx
        .as_ref()
        .map(|security| security.principal.id.as_str())
        .or(ctx.agent_id.as_deref())
        .unwrap_or("");
    match id.strip_prefix("service:").filter(|name| !name.is_empty()) {
        Some(name) => Kind::Service(name.to_string()),
        None => Kind::Outside,
    }
}

fn via_of(ctx: &AgentContext) -> &'static str {
    ctx.security_ctx
        .as_ref()
        .and_then(|security| security.cause.as_ref())
        .map(MoveCause::via)
        .unwrap_or("request")
}

fn principal_id(kind: &Kind) -> String {
    match kind {
        Kind::Outside => "outside".to_string(),
        Kind::Service(name) => format!("service:{name}"),
    }
}

fn remember(
    seen: &mut Seen,
    work: &mut Vec<Pending>,
    entity: &str,
    action: &str,
    ctx: &AgentContext,
) {
    let kind = kind_of(ctx);
    let key = (
        entity.to_string(),
        action.to_string(),
        kind.clone(),
        via_of(ctx),
    );
    if seen.insert(key) {
        work.push(Pending {
            entity: entity.to_string(),
            action: action.to_string(),
            kind,
            cause: ctx
                .security_ctx
                .as_ref()
                .and_then(|security| security.cause.clone()),
        });
    }
}

fn dedup(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by(|left, right| {
        (
            left.code,
            &left.entity_type,
            &left.action,
            &left.principal,
            &left.after,
            &left.detail,
        )
            .cmp(&(
                right.code,
                &right.entity_type,
                &right.action,
                &right.principal,
                &right.after,
                &right.detail,
            ))
    });
    findings.dedup();
    findings
}

#[cfg(test)]
mod tests;
