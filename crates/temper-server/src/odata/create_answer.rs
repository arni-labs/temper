//! What a create answers with.
//!
//! A create never answers with entity data the caller may not read. When the
//! key already names an entity, nothing is written, and the caller's read
//! decision on that entity decides the answer: the same decision a GET of the
//! key makes, on the entity's stored attributes. A caller who may read it gets
//! the existing entity, so a retried create converges on the same answer; any
//! other caller gets `409 Conflict` with no entity body. A key that names a
//! deleted entity stays taken, because deletion is terminal: every caller gets
//! `409 Conflict` with no body, and the deleted record is not revived. A create
//! that makes a new entity answers with it as before: it holds what the caller
//! sent.
//!
//! A create stores its entity before it writes the query projection and arms
//! the entity's timers, so a create can fail after its entity is stored. A
//! repeat that answers with the existing entity finishes that create: it
//! writes the projection and runs [`finish_create`], as the first create
//! would have.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use super::authz::{READ_ACTION, authorize_read};
use super::response::annotate_entity;
use crate::blobs::hydrate_blob_refs_for_tenant;
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::response::{ODataResponse, odata_error};
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
    /// The caller, whom a timer the create arms fires as.
    pub(super) agent_ctx: &'a AgentContext,
    pub(super) entity_type: &'a str,
    pub(super) set_name: &'a str,
    pub(super) entity_id: &'a str,
}

/// The answer to a create whose key another entity holds, including one a
/// concurrent create took after this create's own check: the entity when the
/// caller may read it, `409 Conflict` otherwise. A deleted entity is never
/// answered with.
///
/// The entity's query projection is written again first. A create stores the
/// entity before its projection, so a create that failed between the two
/// leaves the entity out of collection queries until the repeat writes it.
/// A caller who may read the entity also gets the rest of the create
/// ([`finish_create`]), so its timer is armed even when the first create
/// failed before arming it.
pub(super) async fn taken_key_answer(
    state: &ServerState,
    target: &CreateTarget<'_>,
) -> Result<EntityResponse, Response> {
    let existing = state
        .get_tenant_entity_state(target.tenant, target.entity_type, target.entity_id)
        .await
        .map_err(|error| {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error).into_response()
        })?;
    // Reading the key started an actor on a deleted entity's journal: the key
    // stays taken, and the deleted record is neither answered nor written. The
    // actor is unregistered, not stopped, because another create may hold it.
    if existing.state.is_deleted() {
        state.remove_entity(target.tenant, target.entity_type, target.entity_id);
        return Err(existing_key_conflict());
    }
    state
        .upsert_entity_query_projection(
            target.tenant,
            target.entity_type,
            target.entity_id,
            &existing,
            "create_repeat",
        )
        .await
        .map_err(|error| {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "CreateError", &error).into_response()
        })?;
    let body = serde_json::to_value(&existing.state).unwrap_or_default();
    if !may_read(state, target, &body) {
        return Err(existing_key_conflict());
    }
    finish_create(state, target, &existing).await;
    Ok(existing)
}

/// What a create does once its entity is stored and in the query projection:
/// arm the timer of the state the entity is in, and drop the caches that
/// count entities of its type. Repeating it changes nothing: an entity whose
/// timer is already armed keeps it and its deadline, and an entity whose
/// timer it arms late counts from when the entity entered its state.
pub(super) async fn finish_create(
    state: &ServerState,
    target: &CreateTarget<'_>,
    created: &EntityResponse,
) {
    // Creation is not a dispatch: a record created into a timed first state
    // has its timer armed here.
    state
        .arm_state_timeouts_on_creation(
            target.tenant,
            target.entity_type,
            target.entity_id,
            created,
            target.agent_ctx,
        )
        .await;
    if target.entity_type == "RateLimit" {
        state.clear_commons_rate_limit_cache();
    }
    state.clear_commons_storage_projection_cache_for_entity(target.entity_type);
}

/// [`taken_key_answer`] as a collection create's response.
pub(super) async fn taken_key_response(state: &ServerState, target: &CreateTarget<'_>) -> Response {
    match taken_key_answer(state, target).await {
        Ok(existing) => ODataResponse {
            status: EXISTING_ENTITY_STATUS,
            body: create_answer_body(state, target, &existing).await,
        }
        .into_response(),
        Err(response) => response,
    }
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
