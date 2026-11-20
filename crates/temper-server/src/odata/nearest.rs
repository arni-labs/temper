//! `Temper.Nearest`: the records closest in meaning to one record (ADR-0186).
//!
//! ```text
//! GET /tdata/<Set>/Temper.Nearest(to='<id>',k=10)
//! GET /tdata/<Set>/Temper.Nearest(to='<id>',k=10,filter='Status eq ''Published''')
//! ```
//!
//! Ranks the set's other records by closeness to `to`'s meaning row, then walks the
//! ranking under the same `$filter` evaluation and Cedar list and read gates as any
//! read, until `k` records pass. Each carries `@temper.score`.
//!
//! Older callers may still use `decl`, `vector` and `model` to search a
//! declared vector path during the rollout of meaning search.

mod declared;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_odata::path::ODataPath;
use temper_odata::query::types::QueryOptions;
use temper_runtime::tenant::TenantId;

use super::authz::{LIST_ACTION, READ_ACTION, authorize_read};
use super::common::resolve_entity_type;
use super::query_plane_read::{QueryPlaneReadBudget, QueryPlaneReadRequest, walk_ranked};
use super::read_support::materialize_entity_set_entities;
use crate::meaning::rank::{Ranked, rank_nearest};
use crate::response::odata_error;
use crate::state::ServerState;
use crate::storage::BoxedEventStore;

/// Look up a named argument in the parsed bound-function parameter list.
fn arg<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn bad_request(message: &str) -> Response {
    odata_error(StatusCode::BAD_REQUEST, "InvalidNearestQuery", message).into_response()
}

fn not_found(reference_id: &str) -> Response {
    odata_error(
        StatusCode::NOT_FOUND,
        "ResourceNotFound",
        &format!("reference entity '{reference_id}' not found"),
    )
    .into_response()
}

/// The name of the first OData system query option set on the request, if any.
/// `Temper.Nearest` takes its whole interface as function arguments.
fn unsupported_query_option(options: &QueryOptions) -> Option<&'static str> {
    [
        (options.filter.is_some(), "$filter"),
        (options.select.is_some(), "$select"),
        (options.expand.is_some(), "$expand"),
        (options.orderby.is_some(), "$orderby"),
        (options.top.is_some(), "$top"),
        (options.skip.is_some(), "$skip"),
        (options.count.is_some(), "$count"),
        (options.skiptoken.is_some(), "$skiptoken"),
        (options.search.is_some(), "$search"),
    ]
    .into_iter()
    .find_map(|(set, name)| set.then_some(name))
}

enum NearestFrom<'a> {
    Reference(&'a str),
    Vector(&'a str),
}

/// Handle `GET /<Set>/Temper.Nearest(...)`.
pub(super) async fn handle_nearest(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    parent: &ODataPath,
    params: &[(String, String)],
    query_options: &QueryOptions,
) -> Response {
    let set_name = match parent {
        ODataPath::EntitySet(name) => name.clone(),
        _ => {
            return bad_request(
                "Temper.Nearest is bound to an entity collection, e.g. /Trips/Temper.Nearest(to='<id>')",
            );
        }
    };
    if let Some(unsupported) = unsupported_query_option(query_options) {
        return bad_request(&format!(
            "Temper.Nearest does not accept the OData system query option '{unsupported}'; use the function arguments (to or decl/vector/model, k, filter)"
        ));
    }
    let Some(entity_type) = resolve_entity_type(state, tenant, &set_name) else {
        return odata_error(
            StatusCode::NOT_FOUND,
            "ResourceNotFound",
            &format!("Entity set '{set_name}' not found"),
        )
        .into_response();
    };
    let source = match (arg(params, "to"), arg(params, "vector")) {
        (Some(id), None) => NearestFrom::Reference(id),
        (None, Some(vector)) => NearestFrom::Vector(vector),
        _ => return bad_request("Temper.Nearest takes exactly one of 'to' or 'vector'"),
    };
    let budget = QueryPlaneReadBudget::from_config();
    let k = match arg(params, "k") {
        Some(raw) => match raw.parse::<usize>() {
            Ok(k) if k >= 1 => k.min(budget.max_result_k()),
            _ => return bad_request("'k' must be a positive integer"),
        },
        None => 10usize.min(budget.max_result_k()).max(1),
    };
    let filter = match arg(params, "filter") {
        Some(raw) => match temper_odata::query::filter::parse_filter(raw) {
            Ok(expr) => Some(expr),
            Err(error) => return bad_request(&format!("invalid 'filter': {error}")),
        },
        None => None,
    };

    if let Err(response) = authorize_read(
        state,
        tenant,
        security_ctx,
        LIST_ACTION,
        &entity_type,
        "",
        &serde_json::json!({}),
    ) {
        return *response;
    }

    let walk_options = QueryOptions {
        filter,
        ..QueryOptions::default()
    };
    let request = QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx,
        entity_type: &entity_type,
        entity_set_name: &set_name,
        query_options: &walk_options,
        budget,
    };
    let ranked = match source {
        NearestFrom::Reference(id) => rank_reference(&request, id).await,
        NearestFrom::Vector(vector) => declared::rank(&request, params, vector).await,
    };
    let ranked = match ranked {
        Ok(ranked) => ranked,
        Err(response) => return response,
    };
    let page = walk_ranked(&request, &ranked, 0, k, false, false).await;
    crate::response::ODataResponse {
        status: StatusCode::OK,
        body: serde_json::json!({
            "@odata.context": format!("$metadata#{set_name}"),
            "value": page.entities,
        }),
    }
    .into_response()
}

async fn rank_reference(
    request: &QueryPlaneReadRequest<'_>,
    reference_id: &str,
) -> Result<Vec<Ranked>, Response> {
    // The reference record must exist and be readable before anything about it, its
    // meaning or the ranking it seeds, is disclosed.
    let reference = materialize_entity_set_entities(
        request.state,
        request.tenant,
        request.entity_type,
        request.entity_set_name,
        std::slice::from_ref(&reference_id.to_string()),
        true,
        None,
    )
    .await
    .entities
    .into_iter()
    .next();
    let Some(body) =
        reference.filter(|body| body.get("status").and_then(|s| s.as_str()) != Some("Deleted"))
    else {
        return Err(not_found(reference_id));
    };
    if let Err(response) = authorize_read(
        request.state,
        request.tenant,
        request.security_ctx,
        READ_ACTION,
        request.entity_type,
        reference_id,
        &body,
    ) {
        return Err(*response);
    }

    let store = nearest_store(request.state).ok_or_else(no_store)?;
    let meaning = match store
        .meaning_row(request.tenant.as_str(), request.entity_type, reference_id)
        .await
    {
        Ok(row) => row,
        Err(error) => return Err(read_failed(&error.to_string())),
    };
    let Some((vector, model_tag)) = meaning.and_then(|row| Some((row.vector?, row.model_tag)))
    else {
        return Err(odata_error(
            StatusCode::CONFLICT,
            "MeaningNotReady",
            &format!(
                "reference entity '{reference_id}' has no meaning yet: its text waits for the model, or holds no words"
            ),
        )
        .into_response());
    };
    rank_nearest(
        &store,
        request.tenant.as_str(),
        request.entity_type,
        &vector,
        &model_tag,
        reference_id,
    )
    .await
    .map_err(|error| read_failed(&error.to_string()))
}

fn nearest_store(state: &ServerState) -> Option<BoxedEventStore> {
    state.event_journal().map(|(store, _)| store)
}

fn no_store() -> Response {
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "StorageUnavailable",
        "Temper.Nearest requires a store",
    )
    .into_response()
}

fn read_failed(error: &str) -> Response {
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "ReadUnavailable",
        &format!("meaning rows could not be read: {error}"),
    )
    .into_response()
}
