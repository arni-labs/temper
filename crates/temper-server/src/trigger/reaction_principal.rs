//! Who a reaction runs as, and how its move came about.
//!
//! A reaction runs as the invoking principal, or as the service its rule
//! declares (ADR-0046). Either way its security context carries a
//! [`MoveCause`]: the rule, the move that set it off and the chain's depth.
//! Cedar sees it as `context.via` and the rest, and the journal records it.

use temper_authz::{MoveCause, ReactionHop, SecurityContext};

use crate::request_context::AgentContext;

/// The cause of the reaction `rule_name` makes when `source_action` on
/// `source_entity_type`/`source_entity_id`, made with `invoking_ctx`, sets it
/// off: one hop deeper than that move, in the same timer's chain if any.
pub(super) fn reaction_cause(
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
    invoking_ctx: &AgentContext,
) -> MoveCause {
    MoveCause::reaction(
        ReactionHop {
            rule: rule_name.to_string(),
            action: source_action.to_string(),
            entity_type: source_entity_type.to_string(),
            entity_id: source_entity_id.to_string(),
        },
        invoking_ctx
            .security_ctx
            .as_ref()
            .and_then(|context| context.cause.as_ref()),
    )
}

/// ADR-0046: resolve the dispatch context for a reaction.
///
/// - When the rule declares a principal (`Some(service_name)`), build a
///   synthetic `AgentContext` identifying the named service. Cedar policies
///   can match on `principal.role`, `principal.agent_type`, or
///   `principal.id == Service::"<name>"` — whichever style the tenant
///   prefers. The `agent_type` slot carries the name so the Cedar request
///   sees it via `principal.agent_type == "<name>"`.
/// - When the rule has no principal (`None`), inherit the invoking context
///   directly — the trigger runs as whoever called the source action.
///
/// Either way the reaction carries its own idempotency key, derived from the
/// caller's and the rule's name ([`reaction_idempotency_key`]), and its
/// principal's security context carries `cause`: Cedar's input on how the
/// move came about, and the journal's record of it.
pub(super) fn resolve_trigger_principal(
    declared_principal: Option<&str>,
    invoking_ctx: &AgentContext,
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
    cause: &MoveCause,
) -> AgentContext {
    let mut ctx = match declared_principal {
        Some(service_name) if !service_name.is_empty() => {
            let mut ctx = AgentContext::for_service_inheriting(service_name, invoking_ctx);
            if let Some(security_ctx) = ctx.security_ctx.as_mut() {
                security_ctx.context_attrs.insert(
                    "triggerRule".to_string(),
                    serde_json::Value::String(rule_name.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceEntityType".to_string(),
                    serde_json::Value::String(source_entity_type.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceEntityId".to_string(),
                    serde_json::Value::String(source_entity_id.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerSourceAction".to_string(),
                    serde_json::Value::String(source_action.to_string()),
                );
                security_ctx.context_attrs.insert(
                    "triggerDeclaredPrincipal".to_string(),
                    serde_json::Value::Bool(true),
                );
            }
            ctx
        }
        _ => invoking_ctx.clone(),
    };
    if let Some(security_ctx) = ctx.security_ctx.as_mut() {
        security_ctx.cause = Some(cause.clone());
    }
    ctx.idempotency_key = reaction_idempotency_key(invoking_ctx, rule_name);
    ctx.source = crate::entity_actor::MoveSource::Trigger;
    ctx
}

/// A reaction's idempotency key: the caller's, made the rule's own (ADR-0048).
/// A retry of the source action carries the same caller key, so its reactions
/// are deduplicated too. The caller's key itself would make a reaction on the
/// source's own record (`same_id`) a duplicate of the source action, which
/// that record has just recorded under the key, and the reaction would be
/// answered as done without running.
fn reaction_idempotency_key(invoking_ctx: &AgentContext, rule_name: &str) -> Option<String> {
    invoking_ctx
        .idempotency_key
        .as_ref()
        .map(|key| format!("{key}:reaction:{rule_name}"))
}

/// The security context a reaction is checked with: its dispatch context's
/// own, which carries `cause`, or one approximated from the dispatch
/// context's agent fields when it has none, with `cause` set on it.
pub(super) fn effective_trigger_security_context(
    agent_ctx: &AgentContext,
    cause: &MoveCause,
) -> SecurityContext {
    if let Some(security_ctx) = &agent_ctx.security_ctx {
        return security_ctx.clone();
    }

    let mut security_ctx = SecurityContext::anonymous().with_agent_context(
        agent_ctx.agent_id.as_deref(),
        agent_ctx.session_id.as_deref(),
        agent_ctx.agent_type.as_deref(),
    );
    security_ctx.context_attrs.insert(
        "triggerInheritedContextApproximate".to_string(),
        serde_json::Value::Bool(true),
    );
    security_ctx.cause = Some(cause.clone());
    security_ctx
}
