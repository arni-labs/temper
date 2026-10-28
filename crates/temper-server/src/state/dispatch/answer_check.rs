//! Cedar's decision on a module's answer, as the calling move's identity.
//!
//! A module's answer (or an adapter's) runs as the identity of the move that
//! called it, and Cedar decides it as that identity, with `context.via ==
//! "module"` and the trigger as `context.rule`, as it decides a reaction as
//! its sender. This step of the rollout records the decision and does not
//! act on it: an answer Cedar would deny is reported once, as the structured
//! warn event `module_answer_would_deny`, and is still sent. The event names
//! the record, the answer, the identity and the denial, never the answer's
//! values.

use temper_authz::{AuthzDecision, AuthzDenial, Hop, MoveCause};

use super::WasmEntityRef;
use super::owed_writes::OwedMove;
use crate::state::ServerState;

/// The fixed name of the event an answer Cedar would deny is reported as.
pub(super) const WOULD_DENY_EVENT: &str = "module_answer_would_deny";

/// The hop of an answer that `integration`, a trigger of `calling_action`,
/// gives on `entity`: the trigger, named as a reaction's rule is
/// (`<Type>:<Action>:<trigger>`), and the move that called it.
pub(super) fn answer_hop(
    entity: WasmEntityRef<'_>,
    calling_action: &str,
    integration: &str,
) -> Hop {
    Hop {
        rule: format!("{}:{calling_action}:{integration}", entity.entity_type),
        action: calling_action.to_string(),
        entity_type: entity.entity_type.to_string(),
        entity_id: entity.entity_id.to_string(),
    }
}

impl ServerState {
    /// Ask Cedar whether `sent`, a module's answer through the trigger `hop`
    /// names, may be made as its identity, and report a denial. The answer is
    /// sent whatever Cedar says.
    pub(super) async fn check_answer(&self, sent: &OwedMove, hop: &Hop) {
        if !sent.action.is_empty() {
            return;
        }
        let cause = MoveCause::answer(hop.clone(), None);
        let security_ctx =
            crate::trigger::effective_trigger_security_context(&sent.agent_ctx, &cause);
        let snapshot = match self
            .load_authz_resource_snapshot(&sent.tenant, &sent.entity_type, &sent.entity_id)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                tracing::warn!(
                    tenant = %sent.tenant,
                    entity_type = %sent.entity_type,
                    entity_id = %sent.entity_id,
                    action = %sent.action,
                    rule = %hop.rule,
                    %error,
                    "module answer not checked: its record could not be read"
                );
                return;
            }
        };
        let resource = snapshot.resource_attrs.into_iter().collect(); // determinism-ok: Cedar API requires HashMap
        let decision = self.authz.authorize_for_tenant_or_bypass(
            sent.tenant.as_str(),
            &security_ctx,
            &sent.action,
            &sent.entity_type,
            &resource,
        );
        let AuthzDecision::Deny(denial) = decision else {
            return;
        };
        let policy_ids = match &denial {
            AuthzDenial::PolicyDenied { policy_ids } => policy_ids.join(","),
            _ => String::new(),
        };
        let principal = &security_ctx.principal;
        let agent_type_verified = principal
            .attributes
            .get("agentTypeVerified")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        tracing::warn!(
            evt = WOULD_DENY_EVENT,
            tenant = %sent.tenant,
            entity_type = %sent.entity_type,
            entity_id = %sent.entity_id,
            action = %sent.action,
            principal_id = %principal.id,
            principal_kind = ?principal.kind,
            agent_type = principal.agent_type.as_deref().unwrap_or(""),
            agent_type_verified,
            rule = %hop.rule,
            reason = %denial,
            policy_ids = %policy_ids,
            "module answer would be denied"
        );
    }
}
