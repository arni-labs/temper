//! Shared Cedar enforcement for OData entity operations.

use std::collections::BTreeMap;

use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;

use crate::authz::{DenialInput, record_authz_denial};
use crate::request_context::AgentContext;
use crate::response::{odata_denial, odata_error};
use crate::state::ServerState;

pub(super) const CREATE_ACTION: &str = "create";
pub(crate) const LIST_ACTION: &str = "list";
pub(crate) const READ_ACTION: &str = "read";
pub(super) const UPDATE_ACTION: &str = "update";
pub(super) const DELETE_ACTION: &str = "delete";

/// Require the immutable context installed by credential authentication.
#[derive(Clone, Copy, Debug)]
pub(super) struct AuthenticationRequired;

impl IntoResponse for AuthenticationRequired {
    fn into_response(self) -> Response {
        odata_error(
            StatusCode::UNAUTHORIZED,
            "AuthenticationRequired",
            "A valid tenant credential is required",
        )
        .into_response()
    }
}

pub(super) fn require_authenticated_context(
    context: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<AuthenticatedRequestContext, AuthenticationRequired> {
    context
        .map(|Extension(context)| context)
        .ok_or(AuthenticationRequired)
}

/// Attach exact authenticated authority to downstream dispatch context.
///
/// Correlation metadata remains header-derived, but no principal field is
/// reconstructed or enriched from headers.
pub(super) fn apply_authenticated_context(
    agent_context: &mut AgentContext,
    security_context: &SecurityContext,
) {
    agent_context.security_ctx = Some(security_context.clone());
    if matches!(
        security_context.principal.kind,
        temper_authz::PrincipalKind::Agent | temper_authz::PrincipalKind::Admin
    ) {
        agent_context.agent_id = Some(security_context.principal.id.clone());
        agent_context.agent_type = security_context.principal.agent_type.clone();
    }
}

/// Flatten an entity representation into the resource attributes supplied to Cedar.
///
/// Read responses wrap application fields below `fields`, while create payloads
/// supply fields at the top level. Supporting both shapes keeps policy
/// evaluation identical across CRUD paths.
pub(super) fn resource_attrs_from_body(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    resource_id: &str,
    body: &serde_json::Value,
) -> BTreeMap<String, serde_json::Value> {
    let mut attrs = BTreeMap::new();
    let status = body
        .get("status")
        .or_else(|| body.get("Status"))
        .or_else(|| body.get("fields").and_then(|fields| fields.get("status")))
        .or_else(|| body.get("fields").and_then(|fields| fields.get("Status")))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::String(String::new()));

    if let Some(fields) = body.get("fields").and_then(serde_json::Value::as_object) {
        for (key, value) in fields {
            if !temper_spec::automaton::is_server_derived_field_name(key) {
                attrs.insert(key.clone(), value.clone());
            }
        }
    } else if let Some(fields) = body.as_object() {
        for (key, value) in fields {
            if !key.starts_with('@') && !temper_spec::automaton::is_server_derived_field_name(key) {
                attrs.insert(key.clone(), value.clone());
            }
        }
    }

    for key in ["id", "Id"] {
        attrs.insert(
            key.to_string(),
            serde_json::Value::String(resource_id.to_string()),
        );
    }
    for key in ["status", "Status"] {
        attrs.insert(key.to_string(), status.clone());
    }

    let has_spec = state
        .has_registered_spec(tenant, entity_type)
        .expect("registry lock poisoned while building OData authorization attributes");
    attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));
    attrs
}

pub(crate) fn entity_id_from_body(body: &serde_json::Value) -> Option<&str> {
    body.get("entity_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| body.get("Id").and_then(serde_json::Value::as_str))
        .or_else(|| {
            body.get("fields")
                .and_then(|fields| fields.get("Id"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            body.get("fields")
                .and_then(|fields| fields.get("id"))
                .and_then(serde_json::Value::as_str)
        })
}

pub(crate) fn authorize_read(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    body: &serde_json::Value,
) -> Result<(), Box<Response>> {
    let attrs = resource_attrs_from_body(state, tenant, entity_type, entity_id, body);
    state
        .authorize_with_context(security_ctx, action, entity_type, &attrs, tenant.as_str())
        .map_err(|denial| {
            let mut response = odata_error(
                StatusCode::FORBIDDEN,
                "AuthorizationDenied",
                &denial.to_string(),
            )
            .into_response();
            response
                .extensions_mut()
                .insert(crate::authz::DeniedResource {
                    action: action.to_string(),
                    resource_type: entity_type.to_string(),
                    resource_id: entity_id.to_string(),
                    resource_attrs: attrs,
                    reason: denial.to_string(),
                });
            Box::new(response)
        })
}

pub(super) struct MutationResource<'a> {
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) attrs: &'a BTreeMap<String, serde_json::Value>,
}

pub(super) async fn authorize_mutation(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    agent_ctx: &AgentContext,
    action: &str,
    resource: MutationResource<'_>,
) -> Result<(), Response> {
    let MutationResource {
        entity_type,
        entity_id,
        attrs,
    } = resource;
    let Err(denial) =
        state.authorize_with_context(security_ctx, action, entity_type, attrs, tenant.as_str())
    else {
        return Ok(());
    };

    let reason = denial.to_string();
    let decision = record_authz_denial(
        state,
        DenialInput {
            tenant: tenant.as_str(),
            security_ctx,
            agent_id_override: agent_ctx.agent_id.as_deref(),
            action,
            resource_type: entity_type,
            resource_id: entity_id,
            resource_attrs: serde_json::to_value(attrs).unwrap_or_default(),
            reason: &reason,
            module_name: None,
            from_status: attrs
                .get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            intent: agent_ctx.intent.clone(),
            session_id: agent_ctx.session_id.clone(),
            // A genuine attempted dispatch of a registered action: walked by
            // conformance, matching both parents' behavior.
            spec_governed: None,
        },
    )
    .await;

    Err(odata_denial(
        &format!("{reason} (decision: {})", decision.id),
        &decision.id,
    )
    .into_response())
}

/// An outside call of a bound action: the entity as it is, and the body sent.
pub(super) struct BoundActionCall<'a> {
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) action: &'a str,
    pub(super) status: &'a str,
    pub(super) fields: &'a serde_json::Value,
    pub(super) attrs: &'a BTreeMap<String, serde_json::Value>,
    pub(super) body: &'a serde_json::Value,
}

/// An action body that writes fields its action does not declare changes the
/// entity the way a PATCH does, so it needs the same two `update` decisions a
/// PATCH needs: on the entity as it is and as it would be. A body of declared
/// params needs neither. A denial is recorded like any other, so a person can
/// approve it.
pub(super) async fn authorize_undeclared_field_write(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    agent_ctx: &AgentContext,
    call: BoundActionCall<'_>,
) -> Result<(), Response> {
    let Ok(table) = state.transition_table_for_dispatch(tenant, call.entity_type) else {
        // Dispatch answers for a type without a table.
        return Ok(());
    };
    let Some(fields) = crate::entity_actor::undeclared_params::fields_after_undeclared_write(
        &table,
        call.action.rsplit('.').next().unwrap_or(call.action),
        call.fields,
        call.body,
    ) else {
        return Ok(());
    };
    let prospective = state
        .build_authz_resource_attrs(
            tenant,
            call.entity_type,
            call.entity_id,
            call.status,
            &fields,
        )
        .await
        .map_err(|error| {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error).into_response()
        })?;
    for attrs in [call.attrs, &prospective] {
        authorize_mutation(
            state,
            tenant,
            security_ctx,
            agent_ctx,
            UPDATE_ACTION,
            MutationResource {
                entity_type: call.entity_type,
                entity_id: call.entity_id,
                attrs,
            },
        )
        .await?;
    }
    Ok(())
}
