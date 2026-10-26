//! Approval metadata carried only by an explicitly denied HTTP response.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use super::{DenialInput, record_authz_denial};
use crate::state::ServerState;

/// What a refused request asked for.
///
/// Only a refused change becomes a decision someone can approve. A read, or a
/// question about what would be allowed, writes nothing: asking must never
/// load the store, however often a page asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Asked {
    /// To see something, without changing it.
    Read,
    /// To change something.
    Change,
}

impl Asked {
    /// What an HTTP request asks for, by its method: a safe method only reads.
    pub fn of(method: &Method) -> Self {
        if method.is_safe() {
            Self::Read
        } else {
            Self::Change
        }
    }
}

/// A denied resource check, before deciding whether the request needs approval.
///
/// This may be carried in response extensions through read/filter code. A
/// discarded row-filter denial must never be recorded as a requested action.
#[derive(Clone, Debug)]
pub struct DeniedResource {
    /// What the refused request asked for.
    pub asked: Asked,
    /// Cedar action that was denied.
    pub action: String,
    /// Cedar resource type.
    pub resource_type: String,
    /// Exact resource identifier checked by Cedar.
    pub resource_id: String,
    /// Attributes used in that check, retained for decision replay.
    pub resource_attrs: BTreeMap<String, serde_json::Value>,
    /// Original Cedar denial reason.
    pub reason: String,
}

impl DeniedResource {
    /// Answer a denied request. A change an interactive agent asked for is
    /// recorded as a decision, preserving its authenticated identity and
    /// telemetry; a read is only refused. This never grants access or changes
    /// a policy.
    pub async fn for_request(
        self,
        state: &ServerState,
        authenticated: &AuthenticatedRequestContext,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let security_ctx = authenticated.security_context();
        let mut body = serde_json::json!({"error": {
            "code": "AuthorizationDenied", "message": self.reason,
        }});
        let session_id = authenticated.session_id().or_else(|| {
            security_ctx
                .context_attrs
                .get("sessionId")
                .and_then(|value| value.as_str())
        });
        if self.asked == Asked::Change
            && security_ctx.principal.kind == PrincipalKind::Agent
            && session_id.is_some()
        {
            let decision = record_authz_denial(
                state,
                DenialInput {
                    tenant: authenticated.tenant().as_str(),
                    security_ctx,
                    agent_id_override: None,
                    action: &self.action,
                    resource_type: &self.resource_type,
                    resource_id: &self.resource_id,
                    resource_attrs: serde_json::json!(self.resource_attrs),
                    reason: &self.reason,
                    module_name: None,
                    from_status: None,
                    intent: authenticated.intent().map(str::to_string),
                    session_id: session_id.map(str::to_string),
                    spec_governed: Some(false),
                },
            )
            .await;
            body["error"]["message"] =
                serde_json::json!(format!("{} (decision: {})", self.reason, decision.id));
            body["decision_id"] = serde_json::json!(decision.id);
        }
        (StatusCode::FORBIDDEN, Json(body))
    }
}

/// Complete only a denial that escaped read filtering as the HTTP response.
/// Successful reads and discarded row denials cannot generate decisions here.
pub(crate) async fn resolve_requested_denial(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    mut response: Response,
) -> Response {
    if response.status() == StatusCode::FORBIDDEN
        && let Some(denial) = response.extensions_mut().remove::<DeniedResource>()
    {
        return denial
            .for_request(state, authenticated)
            .await
            .into_response();
    }
    response
}
