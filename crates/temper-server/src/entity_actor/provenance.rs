//! Who made a move and which of the kernel's dispatch paths carried it,
//! recorded on every [`EntityEvent`](super::EntityEvent).
//!
//! Both are taken from what the kernel itself knows when it dispatches: the
//! principal its credential resolution produced, and the path in its own code
//! the move went through. Nothing a caller declares is recorded.

use serde::{Deserialize, Serialize};
use temper_authz::{PrincipalKind, SecurityContext, TokenKind};

/// The principal that made a move, as its [`SecurityContext`] has it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MadeBy {
    /// The principal's id: an API key's agent instance id, an access token's
    /// `client_id`, a human token's `sub`, or a platform service's name.
    pub id: String,
    /// The principal's kind.
    pub kind: PrincipalKind,
    /// The principal's agent type, when it has one.
    pub agent_type: Option<String>,
    /// Whether the kernel verified the agent type from a credential rather
    /// than taking it from internal, self-declared context.
    pub agent_type_verified: bool,
    /// The principal this one acts for.
    pub acting_for: Option<String>,
    /// The principal's role.
    pub role: Option<String>,
    /// The trusted issuer that verified the principal's token. None for an
    /// API key and for the kernel's own callers.
    pub issuer: Option<String>,
    /// Whether that token was the human's own or an agent's access token.
    pub token: Option<TokenKind>,
    /// The verified token's `client_id`, when it carried one.
    pub client_id: Option<String>,
    /// The verified token's `grant_id`, when it carried one.
    pub grant_id: Option<String>,
}

impl From<&SecurityContext> for MadeBy {
    fn from(context: &SecurityContext) -> Self {
        let principal = &context.principal;
        let token = context.token.as_ref();
        Self {
            id: principal.id.clone(),
            kind: principal.kind.clone(),
            agent_type: principal.agent_type.clone(),
            agent_type_verified: principal
                .attributes
                .get("agentTypeVerified")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            acting_for: principal.acting_for.clone(),
            role: principal.role.clone(),
            issuer: token.map(|token| token.issuer.clone()),
            token: token.map(|token| token.kind),
            client_id: token.and_then(|token| token.client_id.clone()),
            grant_id: token.and_then(|token| token.grant_id.clone()),
        }
    }
}

/// The kernel's dispatch path that carried a move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveSource {
    /// A request through the HTTP API.
    Http,
    /// A state timer firing (ADR-0056).
    StateTimer,
    /// An action a transition scheduled to run after a delay.
    Scheduled,
    /// A WASM module's callback or write.
    Module,
    /// An action trigger reacting to another move (ADR-0046).
    Trigger,
    /// A child record spawned by another record's move.
    Spawn,
    /// An inbound webhook.
    Webhook,
    /// The kernel's own work: boot, installs, platform services.
    #[default]
    Platform,
}

/// Who made a move and the path that carried it, as the dispatch that sends
/// it to the record knows them. Unknown on a record spawned by a read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MoveOrigin {
    /// The principal that made the move, when the dispatch carried one.
    pub made_by: Option<MadeBy>,
    /// The dispatch path that carried the move.
    pub source: Option<MoveSource>,
}

impl MoveOrigin {
    /// The origin of a move dispatched with `agent_ctx`.
    pub fn of(agent_ctx: &crate::request_context::AgentContext) -> Self {
        Self {
            made_by: agent_ctx.security_ctx.as_ref().map(MadeBy::from),
            source: Some(agent_ctx.source),
        }
    }

    /// The origin of the kernel's own in-process work.
    pub fn platform() -> Self {
        Self::of(&crate::request_context::AgentContext::system())
    }

    /// Record this origin on an event the move wrote.
    pub(crate) fn stamp(&self, event: &mut super::EntityEvent) {
        event.made_by.clone_from(&self.made_by);
        event.source = self.source;
    }
}
