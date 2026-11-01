//! `POST /api/os-apps/{name}/install`: install an app's files, pushed by the
//! caller, into this running server through the one installer
//! ([`crate::genesis_install::install_pushed_app`]).
//!
//! Body: `{"files": {"<path in the app>": "<text>" | {"base64": "..."} |
//! {"keep": "<sha256 hex>"}}, "budget_ms": <time each spec's check may
//! take>}`. The app goes into the credential's own tenant; a request cannot
//! name another. Answers 200 with the install, 400 for a wrong request, 422
//! when a spec's check or the installer refused it, with why. Every answer
//! says how long the push waited for this host's install turn (`waited_ms`).

use std::collections::BTreeMap;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{MethodRouter, post};
use serde::Deserialize;
use temper_authz::AuthenticatedRequestContext;
use temper_server::observe::{DEFAULT_BUDGET_MS, MAX_BUDGET_MS};

use super::auth::{
    PlatformResourceAuthorization, require_authenticated, require_requested_resource_authorization,
};
use super::authorization_error;
use crate::genesis_install::{PushRefused, PushedFile, install_pushed_app};
use crate::state::PlatformState;

/// The largest push this server reads. A push is held in memory while it is
/// decoded and checked, and any credential of the tenant can send one before
/// Cedar decides on it, so its size is bounded: by the largest body this
/// server already takes in one request, a blob upload.
const PUSH_BODY_LIMIT_BYTES: usize = 128 * 1024 * 1024;

/// The body of a push.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushRequest {
    /// Every file of the app, by its path in the app.
    files: BTreeMap<String, PushedFile>,
    /// Time each spec's check may take, in milliseconds.
    #[serde(default)]
    budget_ms: Option<u64>,
}

/// The route, with the body limit a push needs.
pub(super) fn route() -> MethodRouter<PlatformState> {
    post(install).layer(DefaultBodyLimit::max(PUSH_BODY_LIMIT_BYTES))
}

async fn install(
    State(state): State<PlatformState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(name): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let authenticated = match require_authenticated(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return authorization_error(status),
    };
    let tenant = authenticated.tenant().as_str();
    if let Err(denial) = require_requested_resource_authorization(
        &state,
        authenticated,
        PlatformResourceAuthorization {
            action: "install_app",
            resource_type: "App",
            resource_id: &name,
            attrs: BTreeMap::from([(
                "targetTenant".to_string(),
                serde_json::Value::String(tenant.to_string()),
            )]),
        },
    )
    .await
    {
        return denial;
    }

    let request: PushRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => return refusal(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let budget_ms = request.budget_ms.unwrap_or(DEFAULT_BUDGET_MS);
    if !(1..=MAX_BUDGET_MS).contains(&budget_ms) {
        return refusal(
            StatusCode::BAD_REQUEST,
            format!("budget_ms must be between 1 and {MAX_BUDGET_MS}"),
        );
    }

    let budget = Duration::from_millis(budget_ms);
    let (waited, pushed) = install_pushed_app(&state, tenant, &name, request.files, budget).await;
    let (status, mut answer) = match pushed {
        Ok(pushed) => (
            StatusCode::OK,
            serde_json::json!({
                "installed": name,
                "digest": pushed.digest,
                "result": pushed.result,
                "checks": pushed.checks,
            }),
        ),
        Err(PushRefused::Request(error)) => (StatusCode::BAD_REQUEST, error_body(error)),
        Err(PushRefused::Checks(checks)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({
                "error": format!("a spec of '{name}' did not pass its check; nothing was installed"),
                "checks": checks,
            }),
        ),
        Err(PushRefused::Install(error)) => (StatusCode::UNPROCESSABLE_ENTITY, error_body(error)),
        Err(PushRefused::Server(error)) => (StatusCode::INTERNAL_SERVER_ERROR, error_body(error)),
    };
    // How long the push waited for this host's install turn, so a slow answer
    // says whether it was the push or the queue.
    answer["waited_ms"] = serde_json::json!(waited.as_millis() as u64);
    (status, Json(answer))
}

fn error_body(error: String) -> serde_json::Value {
    serde_json::json!({ "error": error })
}

fn refusal(status: StatusCode, error: String) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(error_body(error)))
}

#[cfg(test)]
#[path = "pushed_test.rs"]
mod tests;
