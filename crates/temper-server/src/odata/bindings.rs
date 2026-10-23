//! Bound action helpers for OData write handlers.

use axum::http::StatusCode;
use axum::response::IntoResponse;
use opentelemetry::KeyValue as OtelKeyValue;
use opentelemetry::trace::Status;
use temper_runtime::tenant::TenantId;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use temper_authz::SecurityContext;

use super::account_verification::enforce_commons_account_verified_for_action;
use super::file_content::refuse_caller_content;
use super::rate_limit::enforce_commons_write_rate_limit;
use super::response::annotate_entity;
use crate::authz::{DenialInput, record_authz_denial};
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::request_context::AgentContext;
use crate::response::{ODataResponse, odata_denial, odata_error};
use crate::state::write_record::WriteCaller;
use crate::state::{BoundActionHookContext, DispatchCommand, DispatchError, ServerState};

/// Notes for the request's handler span: OpenTelemetry attributes and status set
/// on the current tracing span.
struct ActionSpan(tracing::Span);

impl ActionSpan {
    fn set_attribute(&self, attribute: OtelKeyValue) {
        self.0.set_attribute(attribute.key, attribute.value);
    }

    fn set_status(&self, status: Status) {
        self.0.set_status(status);
    }
}

fn idempotency_actor_key(tenant: &TenantId, entity_type: &str, entity_id: &str) -> String {
    format!("{tenant}:{entity_type}:{entity_id}")
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_bound_action(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key_str: &str,
    action: &str,
    body_json: serde_json::Value,
    agent_ctx: &AgentContext,
    await_integration: bool,
    idempotency_key: Option<String>,
    security_ctx: &SecurityContext,
) -> axum::response::Response {
    // The bound action's details are attributes of the request's handler span
    // (`POST /odata/{path}`). It is not a span of its own: one request is one
    // server span and one handler span, and only the server span carries the
    // HTTP status (router.rs records it from the response).
    let action_span = ActionSpan(tracing::Span::current());
    for attribute in [
        OtelKeyValue::new("odata.entity_set", set_name.to_string()),
        OtelKeyValue::new("odata.entity_id", key_str.to_string()),
        OtelKeyValue::new("odata.action", action.to_string()),
        OtelKeyValue::new("tenant", tenant.as_str().to_string()),
    ] {
        action_span.set_attribute(attribute);
    }

    if let Some(ref aid) = agent_ctx.agent_id {
        action_span.set_attribute(OtelKeyValue::new("agent.id", aid.clone()));
    }
    if let Some(ref sid) = agent_ctx.session_id {
        action_span.set_attribute(OtelKeyValue::new("session.id", sid.clone()));
    }
    if let Some(ref intent) = agent_ctx.intent {
        action_span.set_attribute(OtelKeyValue::new("intent", intent.clone()));
    }
    for (key, value) in &agent_ctx.observation_metadata {
        action_span.set_attribute(OtelKeyValue::new(
            format!("temper.observation.{key}"),
            value.clone(),
        ));
    }

    let mut dispatch_agent_ctx = agent_ctx.clone();
    dispatch_agent_ctx.security_ctx = Some(security_ctx.clone());

    // Default-deny: reject actions on entity types with no registered spec.
    let is_governed = match state.is_entity_type_governed(tenant, entity_type) {
        Ok(value) => value,
        Err(e) => {
            action_span.set_status(Status::error(e.clone()));
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "RegistryError", &e)
                .into_response();
        }
    };

    if !is_governed {
        action_span.set_status(Status::error("EntityTypeNotGoverned"));
        return odata_error(
            StatusCode::NOT_FOUND,
            "EntityTypeNotGoverned",
            &format!(
                "Entity type '{entity_type}' has no registered spec — actions are denied by default"
            ),
        )
        .into_response();
    }

    let authz_snapshot = match state
        .load_authz_resource_snapshot(tenant, entity_type, key_str)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            action_span.set_status(Status::error(e.clone()));
            let code = if e.contains("registry lock poisoned") {
                "RegistryError"
            } else {
                "ReadError"
            };
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, code, &e).into_response();
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
        return odata_denial(&reason_with_id, &pd.id).into_response();
    }

    if let Err(error) = state.check_verification_gate(tenant, entity_type) {
        action_span.set_status(Status::error("VerificationRequired"));
        return super::common::verification_gate_response(error);
    }

    // Resolve a validation copy before materializing anything. Dispatch resolves
    // the original caller input independently at its own trusted boundary.
    let resolved_body = match state.resolve_authenticated_params(
        tenant,
        entity_type,
        action,
        body_json.clone(),
        &dispatch_agent_ctx,
    ) {
        Ok(params) => params,
        Err(error) => {
            return odata_error(
                StatusCode::BAD_REQUEST,
                "AuthenticatedParameter",
                &error.to_string(),
            )
            .into_response();
        }
    };

    if let Err(resp) = enforce_commons_account_verified_for_action(
        state,
        tenant,
        entity_type,
        &current_state.state.fields,
        &resolved_body,
        security_ctx,
    )
    .await
    {
        action_span.set_status(Status::error("AccountVerificationRequired"));
        return *resp;
    }

    // Relation integrity, field invariants and cross-invariants are checked by
    // dispatch on the record the action stores, as on every other action path.
    if let Some(resp) =
        refuse_caller_content(state, tenant, entity_type, action, &resolved_body).await
    {
        action_span.set_status(Status::error("FileBytesNotStored"));
        return resp;
    }

    if !authz_snapshot.exists {
        let validation = state
            .transition_table_for_dispatch(tenant, entity_type)
            .map_err(|error| error.to_string())
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
            return odata_error(StatusCode::CONFLICT, "StrictActionContract", &error)
                .into_response();
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
        return odata_error(StatusCode::BAD_REQUEST, code, &message).into_response();
    }

    // Charged only once the action contract accepts the request: a refused
    // request spends no write token.
    if let Err(resp) =
        enforce_commons_write_rate_limit(state, tenant, entity_type, security_ctx).await
    {
        action_span.set_status(Status::error("RateLimitExceeded"));
        return resp;
    }

    let snapshot = match state
        .materialize_authorized_snapshot(tenant, entity_type, key_str, authz_snapshot)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return odata_error(
                if matches!(error, DispatchError::Conflict(_)) {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                },
                "AuthorizationStateChanged",
                &error.to_string(),
            )
            .into_response();
        }
    };
    let current_state = snapshot.current_state;
    let expected_authorization_precondition =
        crate::entity_actor::effects::entity_authorization_precondition(&current_state.state);

    // Idempotency cache check
    let actor_key = idempotency_actor_key(tenant, entity_type, key_str);
    if let Some(ref idem_key) = idempotency_key
        && let Some(cached) = state
            .idempotency_cache
            .get_after_effects_applied(&actor_key, idem_key)
    {
        let body = annotate_entity(
            serde_json::to_value(&cached.state).unwrap_or_default(),
            format!("$metadata#{set_name}/$entity"),
            None,
        );
        action_span.set_attribute(OtelKeyValue::new("idempotency.hit", true));
        action_span.set_status(Status::Ok);
        return ODataResponse {
            status: StatusCode::OK,
            body,
        }
        .into_response();
    }

    let result = state
        .dispatch_tenant_action_ext_typed_if_current(
            DispatchCommand {
                tenant,
                entity_type,
                entity_id: key_str,
                action,
                params: body_json.clone(),
                agent_ctx: &dispatch_agent_ctx,
                await_integration,
                await_reactions: true,
            },
            WriteCaller::of_request(security_ctx),
            expected_authorization_precondition,
        )
        .await;

    match result {
        Ok(response) => {
            if response.success {
                // Cache for idempotency
                if let Some(ref idem_key) = idempotency_key {
                    state
                        .idempotency_cache
                        .put_effects_applied(&actor_key, idem_key, &response);
                }

                action_span.set_status(Status::Ok);

                let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
                if let Some(hook) = state.bound_action_hook.as_ref() {
                    match hook
                        .after_bound_action(BoundActionHookContext {
                            state,
                            tenant,
                            entity_type,
                            entity_id: key_str,
                            action,
                            params: &resolved_body,
                            state_json: &state_json,
                        })
                        .await
                    {
                        Ok(Some(hook_json)) => {
                            if let (Some(dst), Some(src)) =
                                (state_json.as_object_mut(), hook_json.as_object())
                            {
                                dst.insert(
                                    "postAction".to_string(),
                                    serde_json::Value::Object(src.clone()),
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            action_span.set_status(Status::error(error.clone()));
                            return odata_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "PostActionHookFailed",
                                &error,
                            )
                            .into_response();
                        }
                    }
                }
                hydrate_blob_refs_for_tenant(state, tenant, &mut state_json).await;
                let body =
                    annotate_entity(state_json, format!("$metadata#{set_name}/$entity"), None);
                ODataResponse {
                    status: StatusCode::OK,
                    body,
                }
                .into_response()
            } else {
                action_span.set_status(Status::error(response.error.clone().unwrap_or_default()));
                odata_error(
                    StatusCode::CONFLICT,
                    "ActionFailed",
                    &response.error.unwrap_or_else(|| "Action failed".into()),
                )
                .into_response()
            }
        }
        Err(DispatchError::Ungoverned(entity)) => {
            let reason = format!(
                "Entity type '{entity}' has no registered spec — actions are denied by default"
            );
            action_span.set_status(Status::error("EntityTypeNotGoverned"));
            odata_error(StatusCode::NOT_FOUND, "EntityTypeNotGoverned", &reason).into_response()
        }
        Err(DispatchError::AuthzDenied(reason)) => {
            action_span.set_status(Status::error(reason.clone()));
            odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", &reason).into_response()
        }
        Err(DispatchError::QuotaExceeded(reason)) => {
            action_span.set_status(Status::error(reason.clone()));
            odata_error(StatusCode::PAYLOAD_TOO_LARGE, "StorageCapExceeded", &reason)
                .into_response()
        }
        Err(DispatchError::Conflict(reason)) => {
            action_span.set_status(Status::error(reason.clone()));
            odata_error(StatusCode::CONFLICT, "Conflict", &reason).into_response()
        }
        // ADR-0048: transient exhaustion → 503 Retry-After so clients and
        // proxies back off instead of paging someone.
        Err(e @ DispatchError::Transient { .. }) => {
            let reason = e.to_string();
            action_span.set_status(Status::error(reason.clone()));
            let mut resp = odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "DispatchTransient",
                &reason,
            )
            .into_response();
            // Retry-After is per-RFC seconds; 1s is a conservative default
            // until admission control (ADR-0051) can supply a tuned value.
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("1"),
            );
            resp
        }
        // ADR-0051: admission control declined; caller should back off.
        Err(DispatchError::Deferred { retry_after_ms }) => {
            let seconds = retry_after_ms.div_ceil(1000).max(1);
            let reason = format!("dispatch deferred: retry after {retry_after_ms}ms");
            action_span.set_status(Status::error("DispatchDeferred"));
            let mut resp =
                odata_error(StatusCode::SERVICE_UNAVAILABLE, "DispatchDeferred", &reason)
                    .into_response();
            let value = axum::http::HeaderValue::from_str(&seconds.to_string())
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("1"));
            resp.headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
            resp
        }
        Err(e) => {
            let reason = e.to_string();
            action_span.set_status(Status::error(reason.clone()));
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "DispatchError", &reason).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_actor_key_matches_actor_persistence_id_shape() {
        let tenant = TenantId::new("acme");

        assert_eq!(
            idempotency_actor_key(&tenant, "WorkCycle", "wc-1"),
            "acme:WorkCycle:wc-1"
        );
    }
}
