//! POST /api/tenants/{tenant}/history/erase — remove deleted entities' history.
//!
//! A deleted entity keeps its whole journal, its snapshots and the trajectories
//! of the requests that changed it. Erasing keeps only the tombstone (without
//! its parameters) and a snapshot holding the id, so the entity still replays as
//! deleted and its sequence numbers do not move. Only who may manage the
//! tenant's policies (the platform) may erase.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tracing::instrument;

use super::PolicyAuthed;
use crate::state::ServerState;

/// At most this many entities per request.
const MAX_ENTITIES: usize = 200;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EraseRequest {
    entities: Vec<EraseTarget>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EraseTarget {
    entity_type: String,
    entity_id: String,
}

#[instrument(skip_all, fields(tenant, otel.name = "POST /api/tenants/{tenant}/history/erase"))]
pub(crate) async fn handle_erase_history(
    State(state): State<ServerState>,
    Path(_tenant): Path<String>,
    auth: PolicyAuthed,
    axum::Json(body): axum::Json<EraseRequest>,
) -> Response {
    if body.entities.is_empty() || body.entities.len() > MAX_ENTITIES {
        return (
            StatusCode::BAD_REQUEST,
            format!("Name 1 to {MAX_ENTITIES} entities"),
        )
            .into_response();
    }
    let tenant = auth.tenant().clone();
    let Some(store) = state.metadata_store_for_tenant(tenant.as_str()).await else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Durable store required").into_response();
    };
    let by = auth.security_context().principal.id.clone();
    let mut results = Vec::with_capacity(body.entities.len());
    for target in &body.entities {
        let outcome = state
            .erase_entity_history(
                &tenant,
                store.as_ref(),
                &target.entity_type,
                &target.entity_id,
            )
            .await;
        tracing::info!(
            tenant = %tenant,
            entity_type = %target.entity_type,
            entity_id = %target.entity_id,
            by = %by,
            ok = outcome.is_ok(),
            "history erase"
        );
        results.push(match outcome {
            Ok(erased) => json!({
                "entity_type": target.entity_type, "entity_id": target.entity_id,
                "status": "erased", "removed": erased,
            }),
            Err(error) => json!({
                "entity_type": target.entity_type, "entity_id": target.entity_id,
                "status": "refused", "error": error,
            }),
        });
    }
    axum::Json(json!({ "tenant": tenant.as_str(), "results": results })).into_response()
}
