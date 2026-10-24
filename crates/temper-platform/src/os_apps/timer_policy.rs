//! Warn when a state timer makes a move the app's Cedar forbids to the
//! people who arm it.
//!
//! A `[[state_timeout]]` follow-on is the app's own rule (ADR-0049): Cedar
//! judges the move into the timed state, and the timer's `on_timeout` move is
//! not checked against Cedar. So when the policies let someone enter the timed
//! state but forbid them the timer's action, the timer makes that move for
//! them. That can be what the author meant, so the install reports it as a
//! warning and does not refuse.
//!
//! What the check can see: it asks Cedar's own evaluator about the principals
//! the statements name. For every string literal in the policy text it tries a
//! `Customer`, `Agent` and `Admin` principal with that id, and an `Agent`
//! acting for it; plus one principal of each kind the statements do not name.
//! Cedar decides which statements apply to the timer's entity type, so a
//! literal from a statement about another type only adds a principal Cedar
//! then judges. The resource carries no attributes, the principal no role, agent
//! type or attributes, and the context only `actingFor`. Conditions on those
//! (record fields, roles, entity hierarchy, other context) evaluate as Cedar
//! evaluates a missing attribute, so a statement that depends on them can be
//! missed or over-reported. Only an explicit `forbid` of the timer's action
//! counts: an action no statement permits is the usual way to say only the
//! timer makes it.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use temper_authz::{
    AuthzDecision, AuthzDenial, AuthzEngine, Principal, PrincipalKind, SecurityContext,
};
use temper_spec::automaton::{Automaton, StateTimeout, parse_automaton};

/// The Cedar action a create is judged as (`temper-server`'s OData create).
const CREATE_ACTION: &str = "create";

/// The id used for a principal the policies do not name. Cedar ids are
/// strings, so this cannot collide with a literal it was compared against
/// unless a statement names this exact text.
const UNNAMED_PRINCIPAL: &str = "a principal the policies do not name";

/// One state timer whose move the app's Cedar forbids to people who arm it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StateTimerPolicyWarning {
    /// The entity type that declares the timer.
    pub entity_type: String,
    /// The timed state.
    pub state: String,
    /// The timer's `on_timeout` action.
    pub on_timeout: String,
    /// The principals Cedar lets enter the state and forbids the action, as
    /// `Kind::"id"` or `Agent acting for "id"`.
    pub principals: Vec<String>,
    /// The moves (or `create`) by which they enter the state.
    pub entered_by: Vec<String>,
    /// The warning in words, for an app author.
    pub message: String,
}

/// The warnings for `specs` (entity type, IOA source) under the tenant's full
/// Cedar `policy_text`. A spec that does not parse or a policy text that does
/// not compile yields none: the install reports those itself.
pub fn state_timer_policy_warnings(
    specs: &[(String, String)],
    policy_text: &str,
) -> Vec<StateTimerPolicyWarning> {
    let timed: Vec<(String, Automaton)> = specs
        .iter()
        .filter_map(|(entity_type, source)| {
            let automaton = parse_automaton(source).ok()?;
            (!automaton.state_timeouts.is_empty()).then(|| (entity_type.clone(), automaton))
        })
        .collect();
    if timed.is_empty() {
        return Vec::new();
    }
    let Ok(engine) = AuthzEngine::new(policy_text) else {
        return Vec::new();
    };
    let candidates = candidate_principals(policy_text);
    let mut warnings = Vec::new();
    for (entity_type, automaton) in &timed {
        for timeout in &automaton.state_timeouts {
            let entering = entering_actions(automaton, timeout);
            if entering.is_empty() {
                continue;
            }
            let mut principals = Vec::new();
            let mut entered_by = BTreeSet::new();
            for candidate in &candidates {
                let allowed: Vec<&String> = entering
                    .iter()
                    .filter(|action| {
                        decide(&engine, &candidate.context, action, entity_type).is_allowed()
                    })
                    .collect();
                if allowed.is_empty() {
                    continue;
                }
                let forbidden = matches!(
                    decide(
                        &engine,
                        &candidate.context,
                        &timeout.on_timeout,
                        entity_type
                    ),
                    AuthzDecision::Deny(AuthzDenial::PolicyDenied { .. })
                );
                if forbidden {
                    principals.push(candidate.label.clone());
                    entered_by.extend(allowed.into_iter().cloned());
                }
            }
            if principals.is_empty() {
                continue;
            }
            let entered_by: Vec<String> = entered_by.into_iter().collect();
            let message = format!(
                "{entity_type}: the state timer on '{state}' makes '{action}' after {secs} s, \
                 and this app's Cedar forbids '{action}' to {who}, who may enter '{state}' by \
                 {how}. The timer makes that move for them: a state timer's move is the app's \
                 rule and Cedar does not check it. If only some people may have the record \
                 move on, limit who may enter '{state}' instead.",
                state = timeout.state,
                action = timeout.on_timeout,
                secs = timeout.after_seconds,
                who = principals.join(", "),
                how = entered_by.join(", "),
            );
            warnings.push(StateTimerPolicyWarning {
                entity_type: entity_type.clone(),
                state: timeout.state.clone(),
                on_timeout: timeout.on_timeout.clone(),
                principals,
                entered_by,
                message,
            });
        }
    }
    warnings
}

/// The Cedar actions that arm the timer: every action whose `to` is the
/// timed state, every `reset_on` action that can leave the record in it (a
/// reset arms the timer again), and `create` when the state is where a record
/// starts. The timer's own action is left out: the timer makes it.
fn entering_actions(automaton: &Automaton, timeout: &StateTimeout) -> Vec<String> {
    let state = timeout.state.as_str();
    let mut actions: BTreeSet<String> = automaton
        .actions
        .iter()
        .filter(|action| action.name != timeout.on_timeout)
        .filter(|action| {
            let enters = action.to.as_deref() == Some(state);
            let resets = timeout.reset_on.contains(&action.name)
                && action.from.iter().any(|from| from == state)
                && action.to.as_deref().is_none_or(|to| to == state);
            enters || resets
        })
        .map(|action| action.name.clone())
        .collect();
    if automaton.automaton.initial == state {
        actions.insert(CREATE_ACTION.to_string());
    }
    actions.into_iter().collect()
}

/// A principal to ask Cedar about, and how a warning names it.
struct Candidate {
    context: SecurityContext,
    label: String,
}

/// The principals the statements name, in each principal kind, plus one each
/// that they do not name.
fn candidate_principals(policy_text: &str) -> Vec<Candidate> {
    let mut literals: BTreeSet<String> = BTreeSet::new();
    if let Ok(statements) = temper_authz::statements::policy_statements(policy_text) {
        for (span, _) in statements {
            literals.extend(string_literals(&policy_text[span]));
        }
    }
    let kinds = [
        (PrincipalKind::Customer, "Customer"),
        (PrincipalKind::Agent, "Agent"),
        (PrincipalKind::Admin, "Admin"),
    ];
    let mut candidates = Vec::new();
    for id in literals
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(UNNAMED_PRINCIPAL))
    {
        for (kind, name) in &kinds {
            candidates.push(Candidate {
                context: security_context(id, kind.clone(), None),
                label: if id == UNNAMED_PRINCIPAL {
                    format!("{name} ({id})")
                } else {
                    format!("{name}::\"{id}\"")
                },
            });
        }
    }
    for id in &literals {
        candidates.push(Candidate {
            context: security_context(UNNAMED_PRINCIPAL, PrincipalKind::Agent, Some(id)),
            label: format!("Agent acting for \"{id}\""),
        });
    }
    candidates
}

/// The string literals in a Cedar statement, unescaped for `\"` and `\\`.
fn string_literals(statement: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut chars = statement.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut literal = String::new();
        while let Some(c) = chars.next() {
            match c {
                '"' => break,
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        literal.push(escaped);
                    }
                }
                other => literal.push(other),
            }
        }
        literals.push(literal);
    }
    literals
}

fn security_context(id: &str, kind: PrincipalKind, acting_for: Option<&str>) -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: id.to_string(),
            kind,
            role: None,
            acting_for: acting_for.map(str::to_string),
            agent_type: None,
            attributes: HashMap::new(), // determinism-ok: SecurityContext uses HashMap
        },
        context_attrs: HashMap::new(), // determinism-ok: SecurityContext uses HashMap
        correlation_id: String::new(),
    }
}

fn decide(
    engine: &AuthzEngine,
    context: &SecurityContext,
    action: &str,
    entity_type: &str,
) -> AuthzDecision {
    let no_attributes: HashMap<String, serde_json::Value> = HashMap::new(); // determinism-ok: the engine's resource attribute map
    engine.review(context, action, entity_type, &no_attributes)
}

/// Log each warning where the install's operators see it.
pub(crate) fn log_warnings(tenant: &str, app_name: &str, warnings: &[StateTimerPolicyWarning]) {
    for warning in warnings {
        tracing::warn!(
            tenant,
            app = %app_name,
            entity_type = %warning.entity_type,
            state = %warning.state,
            on_timeout = %warning.on_timeout,
            principals = ?warning.principals,
            "{}",
            warning.message
        );
    }
}

#[cfg(test)]
#[path = "timer_policy_test.rs"]
mod tests;
