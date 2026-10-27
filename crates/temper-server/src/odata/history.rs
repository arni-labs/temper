//! `GET /tdata/{Set}('{id}')/$history`: a record's transitions, paged.
//!
//! The caller is authorized by the record read itself
//! ([`super::read::load_readable_entity_descriptor`]), so a caller who may
//! not read the record gets exactly the record read's answer (403, or 404
//! for an unknown record) and never a page of its history.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_odata::path::ODataPath;
use temper_runtime::tenant::TenantId;

use super::common::resolve_entity_type;
use super::read::{
    entity_set_not_found_response, load_readable_entity_descriptor, resolve_entity_request_key,
};
use super::read_support::{odata_default_page_size, odata_max_entities};
use crate::entity_history::{HistoryPageRequest, read_history_page};
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

/// The `(default, max)` page size of a history read: the OData collection
/// page budget, since a history page is one in-memory response like any
/// collection page.
pub(crate) fn read_page_budget() -> (usize, usize) {
    (odata_default_page_size(), odata_max_entities())
}

/// Answer a `$history` read on an entity path.
pub(super) async fn handle_entity_history(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    parent: &ODataPath,
    query_params: &std::collections::BTreeMap<String, String>,
) -> Response {
    let ODataPath::Entity(set_name, key) = parent else {
        return odata_error(
            StatusCode::BAD_REQUEST,
            "InvalidPath",
            "$history is read on a single record, e.g. Orders('id')/$history",
        )
        .into_response();
    };
    let request = match HistoryPageRequest::from_query(query_params) {
        Ok(request) => request,
        Err(message) => {
            return odata_error(StatusCode::BAD_REQUEST, "InvalidQuery", &message).into_response();
        }
    };
    let Some(entity_type) = resolve_entity_type(state, tenant, set_name) else {
        return entity_set_not_found_response(state, tenant, set_name).await;
    };
    if state.is_pg_actor_backed(tenant, &entity_type) {
        // The actor-runtime adapter keeps state, not this kernel's journal.
        return odata_error(
            StatusCode::NOT_IMPLEMENTED,
            "HistoryUnavailable",
            &format!("'{set_name}' runs on the actor-runtime adapter, which keeps no journal"),
        )
        .into_response();
    }
    let key = resolve_entity_request_key(state, tenant, &entity_type, key).await;
    if let Err(response) =
        load_readable_entity_descriptor(state, tenant, &entity_type, set_name, &key, security_ctx)
            .await
    {
        return response;
    }

    match read_history_page(state, tenant, &entity_type, &key, request).await {
        Ok(mut body) => {
            if let Some(object) = body.as_object_mut() {
                object.insert(
                    "@odata.context".to_string(),
                    serde_json::json!(format!("$metadata#{set_name}('{key}')/$history")),
                );
            }
            ODataResponse {
                status: StatusCode::OK,
                body,
            }
            .into_response()
        }
        Err(error) => {
            let (status, code) = error.status_and_code();
            odata_error(status, code, &error.message()).into_response()
        }
    }
}
