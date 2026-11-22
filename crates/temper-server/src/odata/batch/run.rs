//! `POST /tdata/$batch`: many writes in one request, each answered on its own.
//!
//! Each row runs through the `/tdata` POST handler, in this process, as the
//! batch's caller: its own Cedar decision, guard, idempotency key, history,
//! triggers and reactions, exactly as if it had been sent alone. Rows are not
//! one transaction: one row's refusal leaves the others as they ran.
//!
//! What the batch saves is per request, not per row: one round trip from the
//! caller, and on a server whose store has a remote copy one push of every
//! row's change before the answer (temper-cli's `serve::sync_copy`).
//!
//! Rows start in order. A row starts only while the request has time left to
//! answer within [`WRITE_ANSWER_BUDGET`], keeping [`MIN_PUSH_BUDGET`] for the
//! push; a row the batch had no time to start is answered `NotStarted`, with
//! nothing done to it, to be sent again.

use std::time::Duration;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, StreamExt};
use serde_json::{Value, json};
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;
use tracing::{Instrument, instrument};

use super::super::answer_budget::{MIN_PUSH_BUDGET, WRITE_ANSWER_BUDGET};
use super::super::authz::require_authenticated_context;
use super::super::write::handle_odata_post;
use super::super::write_key::IDEMPOTENCY_KEY;
use super::rows::{BatchRow, read_rows};
use crate::response::odata_error;
use crate::state::ServerState;

/// Answer every row of a `$batch` request, in the rows' order.
#[instrument(skip_all, fields(otel.name = "POST /odata/$batch", batch.rows = tracing::field::Empty, batch.not_started = tracing::field::Empty))]
pub async fn handle_odata_batch(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    let rows = match read_rows(&body) {
        Ok(rows) => rows,
        Err(message) => {
            return odata_error(StatusCode::BAD_REQUEST, "InvalidBatch", &message).into_response();
        }
    };
    let row_count = rows.len();
    tracing::Span::current().record("batch.rows", row_count);
    let batch = RowRun {
        state,
        authenticated,
        base_headers: base_headers(headers),
        started: sim_now(),
    };
    // The rows run on a task of their own so a caller that hangs up cannot
    // stop a row between its commit and its effects. It is awaited here, so
    // the batch's order of effects does not change.
    let rows_task = batch.run_all(rows).instrument(tracing::Span::current());
    let task = tokio::spawn(rows_task); // determinism-ok: awaited at once, as batch_delete's task is
    let answers = match task.await {
        Ok(answers) => answers,
        Err(error) => {
            return odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "BatchFailed",
                &format!("the batch's task ended without an answer: {error}"),
            )
            .into_response();
        }
    };
    debug_assert_eq!(answers.len(), row_count, "every row is answered");
    let not_started = answers
        .iter()
        .filter(|answer| answer["body"]["error"]["code"] == NOT_STARTED)
        .count();
    tracing::Span::current().record("batch.not_started", not_started);
    (StatusCode::OK, axum::Json(json!({ "responses": answers }))).into_response()
}

/// The code of a row the batch had no time to start.
const NOT_STARTED: &str = "NotStarted";

/// What every row of one batch runs with.
#[derive(Clone)]
struct RowRun {
    state: ServerState,
    authenticated: AuthenticatedRequestContext,
    /// The batch request's headers, which every row carries under its own.
    base_headers: HeaderMap,
    started: chrono::DateTime<chrono::Utc>,
}

impl RowRun {
    /// Run `rows` in order, as many at once as the runtime has workers to run
    /// them, and answer each.
    async fn run_all(self, rows: Vec<BatchRow>) -> Vec<Value> {
        let at_once = rows_at_once();
        stream::iter(rows)
            .map(|row| self.clone().run(row))
            .buffered(at_once)
            .collect()
            .await
    }

    /// Run one row through the POST handler, if the batch still has time.
    async fn run(self, row: BatchRow) -> Value {
        let elapsed = (sim_now() - self.started).to_std().unwrap_or_default();
        if !may_start(elapsed) {
            return not_started(&row);
        }
        let mut headers = self.base_headers;
        headers.extend(row.headers.clone());
        let response = handle_odata_post(
            State(self.state),
            Some(Extension(self.authenticated)),
            headers,
            Path(row.path.clone()),
            Query(row.query.clone()),
            row.body.clone(),
        )
        .await
        .into_response();
        answer(&row, response).await
    }
}

/// Whether a row may start `elapsed` into its batch: only while the request
/// can still answer within its budget, its push's least time kept.
pub(crate) fn may_start(elapsed: Duration) -> bool {
    elapsed.saturating_add(MIN_PUSH_BUDGET) < WRITE_ANSWER_BUDGET
}

/// How many rows run at once: the runtime's workers. A row's actor and its
/// commit run on tasks of their own, so this many keeps every worker busy;
/// more would only queue for a worker, and the store's write gate admits one
/// commit at a time.
fn rows_at_once() -> usize {
    // A current-thread runtime, as in simulation, has one worker, so
    // simulated rows run one at a time.
    tokio::runtime::Handle::try_current() // determinism-ok: one worker in simulation
        .map(|handle| handle.metrics().num_workers())
        .unwrap_or(1)
        .max(1)
}

/// The batch request's headers that each row carries, without the batch's
/// own key and length.
fn base_headers(mut headers: HeaderMap) -> HeaderMap {
    headers.remove(&IDEMPOTENCY_KEY);
    headers.remove(header::CONTENT_LENGTH);
    headers
}

/// A row's answer in the batch: its status, its key and its body.
async fn answer(row: &BatchRow, response: Response) -> Value {
    let status = response.status().as_u16();
    // The handler built the whole body in memory; reading it holds no more.
    let body = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(bytes) if bytes.is_empty() => Value::Null,
        Ok(bytes) => serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) => json!({ "error": { "code": "BatchRowBody", "message": error.to_string() } }),
    };
    json!({
        "id": row.id,
        "status": status,
        "headers": { IDEMPOTENCY_KEY.as_str(): row.key },
        "body": body,
    })
}

/// The answer of a row the batch had no time to start.
fn not_started(row: &BatchRow) -> Value {
    json!({
        "id": row.id,
        "status": StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        "headers": { IDEMPOTENCY_KEY.as_str(): row.key },
        "body": { "error": {
            "code": NOT_STARTED,
            "message": "Not started: the batch had no time left for this row, and nothing was done to it. Send it again.",
        }},
    })
}
