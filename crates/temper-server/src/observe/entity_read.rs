//! One read decision for every observe route that returns a single entity.

use axum::http::StatusCode;
use temper_authz::AuthenticatedRequestContext;

use crate::state::ServerState;

/// Load an existing entity and authorize reading it, or answer `404`.
pub(super) async fn require_existing_entity_read(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), StatusCode> {
    let body = crate::odata::load_existing_entity_descriptor_body(
        state,
        authenticated.tenant(),
        entity_type,
        entity_type,
        entity_id,
    )
    .await
    .map_err(|_| StatusCode::NOT_FOUND)?;
    authorize_entity_read(state, authenticated, entity_type, entity_id, &body)
}

/// Make the same Cedar read decision as `GET /tdata/{Set}('{id}')`.
///
/// The resource is the exact entity id with the entity's fields and status as
/// attributes, so a policy that names one entity or one of its fields decides
/// observe reads exactly as it decides OData reads.
pub(super) fn authorize_entity_read(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    entity_type: &str,
    entity_id: &str,
    body: &serde_json::Value,
) -> Result<(), StatusCode> {
    crate::odata::authz::authorize_read(
        state,
        authenticated.tenant(),
        authenticated.security_context(),
        crate::odata::authz::READ_ACTION,
        entity_type,
        entity_id,
        body,
    )
    .map_err(|_| {
        tracing::warn!(entity_type, entity_id, "observe entity read denied");
        StatusCode::FORBIDDEN
    })
}
