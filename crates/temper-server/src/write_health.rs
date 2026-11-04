//! What a caller hears about the store's writes: `/healthz`, and the answer to
//! a move the store could not take.

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::response::odata_error;
use crate::state::ServerState;
use crate::storage::WriteHealth;

/// `/healthz`: whether this server answers, whether it is still starting, and
/// whether its store takes writes.
///
/// `status` is `starting` while the server's boot runs (see
/// [`ServerState::mark_starting`]), with `boot` naming the boot step it is in,
/// its tenant and how long it has been in it ([`crate::boot_steps`]); then
/// `serving`. A starting server answers 200: it is up and making its way, and
/// a supervisor that restarts a process after failed asks would only start
/// the boot again. Nothing ends a start that stays starting; its step says
/// where it is. An answer with no `status` is from a server that answered only
/// once its boot had ended, so it serves.
///
/// It answers 503 while the store's appends fail, from the first failed append
/// until one lands, so a supervisor that restarts a process after failed asks
/// starts a store that failed inside this process again; nothing else
/// recovers it. A refusal for room answers 200 with the refusal in the body: a
/// restart makes no room (growing the disk does), and while it starts this
/// server serves no reads either.
///
/// It also counts the tenants not serving: those still starting, restored
/// after the server serves (see [`crate::tenant_restore`]), and those startup
/// could not restore. Neither makes this server unhealthy: a restart would
/// not restore them sooner.
pub async fn healthz(State(state): State<ServerState>) -> Response {
    let health = state
        .event_journal()
        .map_or(WriteHealth::Writing, |(store, _)| store.write_health());
    let (status, mut body) = match health {
        WriteHealth::Writing => (StatusCode::OK, serde_json::json!({ "ok": true })),
        WriteHealth::Refusing(error) => (
            StatusCode::OK,
            serde_json::json!({ "ok": true, "writes": format!("refused: {error}") }),
        ),
        WriteHealth::Failing(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({ "ok": false, "writes": format!("failing: {error}") }),
        ),
    };
    let starting = state.is_starting();
    body["status"] = serde_json::json!(if starting { "starting" } else { "serving" });
    if starting && let Some(step) = state.boot_steps.current() {
        body["boot"] = serde_json::json!(step);
    }
    let unavailable = state
        .registry
        .read()
        .map_or(0, |registry| registry.unavailable_tenants().len());
    body["tenants"] = serde_json::json!({
        "starting": state.tenant_restore.starting_count(),
        "unavailable": unavailable,
    });
    (status, axum::Json(body)).into_response()
}

/// The answer to a move the store could not take: 503 `StoreUnavailable` with
/// the store's own words. Retry-After is one second, as for a dispatch that ran
/// out of tries (ADR-0048): each try costs a refusal and no write.
pub(crate) fn store_unavailable(error: &str) -> Response {
    let mut response =
        odata_error(StatusCode::SERVICE_UNAVAILABLE, "StoreUnavailable", error).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}
