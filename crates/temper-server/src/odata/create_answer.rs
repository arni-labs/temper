//! What a create answers with.
//!
//! A create never answers with entity data the caller may not read. When the
//! key already names an entity, nothing is written, and the caller's read
//! decision on that entity decides the answer: the same decision a GET of the
//! key makes, on the entity's stored attributes. A caller who may read it gets
//! the existing entity, so a retried create converges on the same answer; any
//! other caller gets `409 Conflict` with no entity body. A create that makes a
//! new entity answers with it as before: it holds what the caller sent.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use super::authz::{READ_ACTION, authorize_read};
use super::response::annotate_entity;
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::EntityResponse;
use crate::response::odata_error;
use crate::state::ServerState;

/// Status of a create answered with an entity that already existed and that
/// the caller may read. Nothing was written; the status stays `201 Created`
/// because callers treat a repeated create of a readable entity as the same
/// successful create and check for 201.
pub(super) const EXISTING_ENTITY_STATUS: StatusCode = StatusCode::CREATED;

/// The entity a create's key names.
pub(super) struct CreateTarget<'a> {
    pub(super) tenant: &'a TenantId,
    pub(super) security_ctx: &'a SecurityContext,
    pub(super) entity_type: &'a str,
    pub(super) set_name: &'a str,
    pub(super) entity_id: &'a str,
}

/// The existing entity a create's key names, when there is one.
///
/// `Ok(None)`: no entity has this key and the create goes ahead.
/// `Ok(Some(_))`: the caller may read the existing entity; answer with it.
/// `Err(_)`: `409 Conflict` when the caller may not read it, or the read
/// failure.
pub(super) async fn existing_entity_for_create(
    state: &ServerState,
    target: &CreateTarget<'_>,
) -> Result<Option<EntityResponse>, Response> {
    if !state
        .ensure_entity_loaded(target.tenant, target.entity_type, target.entity_id)
        .await
    {
        return Ok(None);
    }
    let existing = state
        .get_tenant_entity_state(target.tenant, target.entity_type, target.entity_id)
        .await
        .map_err(|error| {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error).into_response()
        })?;
    let body = serde_json::to_value(&existing.state).unwrap_or_default();
    if !may_read(state, target, &body) {
        return Err(existing_key_conflict());
    }
    Ok(Some(existing))
}

/// Whether the caller may read the entity `body` describes: the decision a
/// GET of the key makes.
pub(super) fn may_read(state: &ServerState, target: &CreateTarget<'_>, body: &Value) -> bool {
    authorize_read(
        state,
        target.tenant,
        target.security_ctx,
        READ_ACTION,
        target.entity_type,
        target.entity_id,
        body,
    )
    .is_ok()
}

/// `409 Conflict` for a create whose key already names an entity the caller
/// may not read. It carries no entity data.
pub(super) fn existing_key_conflict() -> Response {
    odata_error(
        StatusCode::CONFLICT,
        "EntityAlreadyExists",
        "An entity with this key already exists",
    )
    .into_response()
}

/// The body a create answers with: the entity, its blob references hydrated.
pub(super) async fn create_answer_body(
    state: &ServerState,
    target: &CreateTarget<'_>,
    response: &EntityResponse,
) -> Value {
    let set_name = target.set_name;
    let entity_id = target.entity_id;
    let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
    hydrate_blob_refs_for_tenant(state, target.tenant, &mut state_json).await;
    annotate_entity(
        state_json,
        format!("$metadata#{set_name}/$entity"),
        Some(format!("{set_name}('{entity_id}')")),
    )
}
