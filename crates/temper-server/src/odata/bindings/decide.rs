//! Every check a bound action passes before its dispatch, decided on the record
//! as it is read now. The route runs them again when the record changed before
//! the action reached it (see `crate::state::redecide`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use opentelemetry::trace::Status;
use temper_authz::SecurityContext;
use temper_jit::table::ContractError;
use temper_runtime::tenant::TenantId;

use super::ActionSpan;
use crate::authz::{DenialInput, record_authz_denial};
use crate::odata::account_verification::enforce_commons_account_verified_for_action;
use crate::odata::common::{authorized_snapshot_failure, run_write_prechecks};
use crate::odata::file_content::refuse_caller_content;
use crate::odata::rate_limit::{enforce_commons_write_rate_limit, owner_id_from_action};
use crate::request_context::AgentContext;
use crate::response::{odata_denial, odata_error, odata_refusal};
use crate::state::{DispatchError, ServerState};

/// One bound action as the caller sent it.
pub(super) struct BoundAction<'a> {
    pub(super) state: &'a ServerState,
    pub(super) tenant: &'a TenantId,
    pub(super) entity_type: &'a str,
    pub(super) key_str: &'a str,
    pub(super) action: &'a str,
    pub(super) body_json: &'a serde_json::Value,
    pub(super) agent_ctx: &'a AgentContext,
    /// `agent_ctx` with the caller's security context, as the dispatch sees it.
    pub(super) dispatch_agent_ctx: &'a AgentContext,
    pub(super) security_ctx: &'a SecurityContext,
    pub(super) span: &'a ActionSpan,
}

/// A bound action every check allowed on one state of its record.
pub(super) struct Decision {
    /// The caller's parameters, resolved to validate them; dispatch resolves
    /// the original input again at its own boundary.
    pub(super) resolved_body: serde_json::Value,
    /// The state the checks read, which the actor commits on or refuses.
    pub(super) decided_on: String,
}

/// A bound action that is not dispatched, and its answer.
pub(super) struct Undecided {
    pub(super) response: Response,
    /// The record changed while it was decided on, so it can be decided again.
    pub(super) record_changed: bool,
}

impl From<Response> for Undecided {
    fn from(response: Response) -> Self {
        Self {
            response,
            record_changed: false,
        }
    }
}

/// Decide `request` on its record as it is now. `first` is whether this is the
/// action's first decision: its rate-limit token is taken only then.
pub(super) async fn decide(request: &BoundAction<'_>, first: bool) -> Result<Decision, Undecided> {
    let BoundAction {
        state,
        tenant,
        entity_type,
        key_str,
        action,
        body_json,
        agent_ctx,
        dispatch_agent_ctx,
        security_ctx,
        span: action_span,
    } = *request;

    let authz_snapshot = match state
        .load_authz_resource_snapshot(tenant, entity_type, key_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            let e = e.to_string();
            action_span.set_status(Status::error(e.clone()));
            let code = if e.contains("registry lock poisoned") {
                "RegistryError"
            } else {
                "ReadError"
            };
            return Err(odata_error(StatusCode::INTERNAL_SERVER_ERROR, code, &e)
                .into_response()
                .into());
        }
    };
    let current_state = &authz_snapshot.current_state;
    let resource_attrs = &authz_snapshot.resource_attrs;

    let authz_result = state.authorize_with_context(
        security_ctx,
        action,
        entity_type,
        resource_attrs,
        tenant.as_str(),
    );
    if let Err(denial) = authz_result {
        let reason = denial.to_string();
        let pd = record_authz_denial(
            state,
            DenialInput {
                tenant: tenant.as_str(),
                security_ctx,
                agent_id_override: agent_ctx.agent_id.as_deref(),
                action,
                resource_type: entity_type,
                resource_id: key_str,
                resource_attrs: serde_json::to_value(resource_attrs).unwrap_or_default(),
                reason: &reason,
                module_name: None,
                from_status: Some(current_state.state.status.clone()),
                intent: agent_ctx.intent.clone(),
                session_id: agent_ctx.session_id.clone(),
                // A genuine attempted dispatch of a registered action: walked by
                // conformance, matching both parents' behavior.
                spec_governed: None,
            },
        )
        .await;

        action_span.set_status(Status::error(reason.clone()));
        let reason_with_id = format!("{reason} (decision: {})", pd.id);
        return Err(odata_denial(&reason_with_id, &pd.id).into_response().into());
    }

    if let Err(error) = state.check_verification_gate(tenant, entity_type) {
        action_span.set_status(Status::error("VerificationRequired"));
        return Err(crate::odata::common::verification_gate_response(error).into());
    }

    // Resolve a validation copy before materializing anything. Dispatch resolves
    // the original caller input independently at its own trusted boundary.
    let resolved_body = match state.resolve_authenticated_params(
        tenant,
        entity_type,
        action,
        body_json.clone(),
        dispatch_agent_ctx,
    ) {
        Ok(params) => params,
        Err(error) => {
            return Err(odata_error(
                StatusCode::BAD_REQUEST,
                "AuthenticatedParameter",
                &error.to_string(),
            )
            .into_response()
            .into());
        }
    };

    if let Err(resp) = enforce_commons_account_verified_for_action(
        state,
        tenant,
        entity_type,
        &current_state.state.fields,
        &resolved_body,
    )
    .await
    {
        action_span.set_status(Status::error("AccountVerificationRequired"));
        return Err((*resp).into());
    }

    if first
        && let Err(resp) = enforce_commons_write_rate_limit(
            state,
            tenant,
            entity_type,
            owner_id_from_action(&current_state.state.fields, &resolved_body),
            security_ctx,
        )
        .await
    {
        action_span.set_status(Status::error("RateLimitExceeded"));
        return Err(resp.into());
    }

    let current_fields = current_state.state.fields.clone();
    if let Err(resp) = run_write_prechecks(
        state,
        tenant,
        entity_type,
        key_str,
        action,
        "bound_action",
        &current_fields,
    )
    .await
    {
        action_span.set_status(Status::error("ConstraintViolation"));
        return Err(resp.into());
    }
    if let Some(resp) = refuse_caller_content(entity_type, action, &resolved_body) {
        action_span.set_status(Status::error("FileContentFields"));
        return Err(resp.into());
    }

    if !authz_snapshot.exists {
        let validation = state
            .transition_table_for_dispatch(tenant, entity_type)
            .map_err(|error| ContractError::message(error.to_string()))
            .and_then(|table| {
                table.validate_action_params(
                    action.rsplit('.').next().unwrap_or(action),
                    &resolved_body,
                    &current_state.state.fields,
                    &current_state.state.counters,
                    &current_state.state.booleans,
                )
            });
        if let Err(error) = validation {
            action_span.set_status(Status::error("StrictActionContract"));
            return Err(odata_refusal(
                StatusCode::CONFLICT,
                "StrictActionContract",
                &error.message,
                error.refusal.as_ref(),
            )
            .into_response()
            .into());
        }
    }
    // After Cedar: when enforcing, a body with params the action does not
    // declare is refused (nerdsane/temper#351).
    if let Ok(table) = state.transition_table_for_dispatch(tenant, entity_type)
        && let Some((code, message)) = crate::entity_actor::undeclared_params::external_refusal(
            &table,
            action.rsplit('.').next().unwrap_or(action),
            &resolved_body,
        )
    {
        action_span.set_status(Status::error(code));
        return Err(odata_error(StatusCode::BAD_REQUEST, code, &message)
            .into_response()
            .into());
    }

    let snapshot = match state
        .materialize_authorized_snapshot(tenant, entity_type, key_str, authz_snapshot)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // A conflict here is the record created, differently than the
            // checks read it, while they ran.
            return Err(Undecided {
                response: authorized_snapshot_failure(&error).into_response(),
                record_changed: matches!(error, DispatchError::Conflict(_)),
            });
        }
    };
    Ok(Decision {
        resolved_body,
        decided_on: crate::entity_actor::effects::entity_authorization_precondition(
            &snapshot.current_state.state,
        ),
    })
}
