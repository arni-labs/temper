//! Who a reaction runs as, and the idempotency key it carries.

use temper_authz::SecurityContext;

use crate::request_context::AgentContext;

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
/// caller's and the rule's name ([`reaction_idempotency_key`]).
pub(super) fn resolve_trigger_principal(
    declared_principal: Option<&str>,
    invoking_ctx: &AgentContext,
    rule_name: &str,
    source_entity_type: &str,
    source_entity_id: &str,
    source_action: &str,
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
    ctx.idempotency_key = reaction_idempotency_key(invoking_ctx, rule_name);
    ctx
}

/// A reaction's idempotency key: the caller's, made the rule's own (ADR-0048).
/// A retry of the source action carries the same caller key, so its reactions
/// are deduplicated too. The caller's key itself would make a reaction on the
/// source's own record (`same_id`) a duplicate of the source action, which
/// that record has just recorded under the key, and the reaction would be
/// answered as done without running.
///
/// The key ends in a digest of the caller's key and the rule, so it is the
/// same length however deep in its chain the reaction is: a chain is as long
/// as its data, every event it writes stores its key, and a key that grew by
/// a suffix at each reaction would make a chain's keys cost the square of its
/// length.
fn reaction_idempotency_key(invoking_ctx: &AgentContext, rule_name: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    invoking_ctx.idempotency_key.as_ref().map(|key| {
        let digest = Sha256::digest(format!("{key}\0{rule_name}").as_bytes());
        format!("reaction:{rule_name}:{digest:x}")
    })
}

/// The security context a reaction is authorized under: the dispatch
/// context's own, or an approximation built from its agent fields.
pub(super) fn effective_trigger_security_context(agent_ctx: &AgentContext) -> SecurityContext {
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
    security_ctx
}
