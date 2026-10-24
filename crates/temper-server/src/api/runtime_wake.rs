//! `POST /api/runtime/wake`: fire what fell due while the process slept.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{
    ResourceAuthorization, require_authenticated_context, require_resource_authorization,
};
use crate::state::ServerState;

/// The one runtime resource a wake acts on, as Cedar sees it.
const RUNTIME_RESOURCE_ID: &str = "runtime";

/// Fire every timer of the caller's tenant that is due by the wall clock, wait
/// for those fires, and answer `{"fired": n, "next_due_at": ...}`.
///
/// An unauthenticated caller gets 401 and one without a Cedar permit for
/// `wake` on `Runtime::"runtime"` gets 403; neither learns when the process
/// acts next. A caller's `traceparent` makes this request's span its child
/// (the router does that for every route).
#[instrument(skip_all, fields(otel.name = "POST /api/runtime/wake"))]
pub(super) async fn handle_wake(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Response {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_resource_authorization(
        &state,
        authenticated,
        ResourceAuthorization {
            action: "wake",
            resource_type: "Runtime",
            resource_id: RUNTIME_RESOURCE_ID,
            resource_attrs: BTreeMap::new(),
        },
    ) {
        return status.into_response();
    }
    let outcome = state.wake_runtime(authenticated.tenant()).await;
    (StatusCode::OK, Json(outcome)).into_response()
}
