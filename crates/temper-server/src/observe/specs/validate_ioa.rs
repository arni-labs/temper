use std::time::Duration;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use temper_authz::AuthenticatedRequestContext;
use tracing::instrument;

use crate::authz::{require_authenticated_context, require_observe_auth};
use crate::state::ServerState;

use super::types::ValidateIoaRequest;

const DEFAULT_SIM_SEEDS: u64 = 5;
const DEFAULT_PROP_TEST_CASES: u32 = 100;
const MAX_SIM_SEEDS: u64 = 100;
const MAX_PROP_TEST_CASES: u32 = 10_000;
const MAX_IOA_SOURCE_BYTES: usize = 1_048_576;
/// Time one check may take when the caller names no budget. A WASM
/// integration waits at most 50 s on one call, so a check that runs out of
/// this still answers its caller, with why.
const DEFAULT_BUDGET_MS: u64 = 30_000;
/// The most time a caller may give one check.
const MAX_BUDGET_MS: u64 = 120_000;
/// States one check's model check may generate, a bound on its memory. A
/// state reached twice counts twice: Katagami's own KatagamiApp generates
/// 5.3M (381k distinct) in 14 s.
const BUDGET_STATES: usize = 10_000_000;

/// POST /api/specs/validate-ioa -- validate IOA source without loading it.
///
/// This lets agents lint locally, then ask the running server/kernel to perform
/// the authoritative L0-L3 cascade before attempting a spec load.
///
/// The source is anyone's, and a few counters can make its check run for
/// hours on a thread nothing can cancel, so every check has a budget
/// (`budget_ms`, default 30 s). A check that runs out answers 200 with the
/// level it was in failed, saying so, and `stopped` naming the type.
#[instrument(skip_all, fields(otel.name = "POST /api/specs/validate-ioa"))]
pub(crate) async fn handle_validate_ioa(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Json(body): Json<ValidateIoaRequest>,
) -> Result<Json<temper_verify::CascadeResult>, (StatusCode, String)> {
    let authenticated = require_authenticated_context(authenticated.as_deref())
        .map_err(|status| (status, "authentication required".to_string()))?;
    require_observe_auth(&state, authenticated, "run_verification", "Verification")
        .map_err(|status| (status, "verification authorization failed".to_string()))?;

    let ioa_source = body.ioa_source;
    if ioa_source.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "ioa_source must not be empty".to_string(),
        ));
    }
    if ioa_source.len() > MAX_IOA_SOURCE_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("ioa_source exceeds {MAX_IOA_SOURCE_BYTES} bytes"),
        ));
    }

    let sim_seeds = body.sim_seeds.unwrap_or(DEFAULT_SIM_SEEDS);
    if sim_seeds > MAX_SIM_SEEDS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("sim_seeds must be <= {MAX_SIM_SEEDS}"),
        ));
    }

    let prop_test_cases = body.prop_test_cases.unwrap_or(DEFAULT_PROP_TEST_CASES);
    if prop_test_cases > MAX_PROP_TEST_CASES {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("prop_test_cases must be <= {MAX_PROP_TEST_CASES}"),
        ));
    }

    let budget_ms = body.budget_ms.unwrap_or(DEFAULT_BUDGET_MS);
    if !(1..=MAX_BUDGET_MS).contains(&budget_ms) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("budget_ms must be between 1 and {MAX_BUDGET_MS}"),
        ));
    }
    let budget = temper_verify::Budget {
        time: Duration::from_millis(budget_ms),
        states: BUDGET_STATES,
    };

    let result = tokio::task::spawn_blocking(move || {
        temper_verify::VerificationCascade::from_ioa(&ioa_source)
            .with_sim_seeds(sim_seeds)
            .with_prop_test_cases(prop_test_cases)
            .with_budget(budget)
            .run()
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("verification worker failed: {error}"),
        )
    })?;

    if let Some(stopped) = &result.stopped {
        tracing::warn!(budget_ms, stopped = %stopped, "validate-ioa ran out of its budget");
    }
    Ok(Json(result))
}
