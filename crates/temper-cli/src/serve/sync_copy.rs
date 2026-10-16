//! Answers wait for the database's Turso Cloud copy (Turso Sync).
//!
//! With `TURSO_SYNC_URL` set, the database stays a local file and every
//! request that can change it is answered only after its changes reach the
//! remote copy. Requests that finish together share one push. A background
//! loop also pushes changes no request waits for (timers, reactions) and keeps
//! the local write-ahead log short.

use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_store_turso::SyncCopy;

/// How long an answer keeps retrying its push before the change is reported
/// as not confirmed.
const ANSWER_PUSH_BUDGET: Duration = Duration::from_secs(10);

/// Default period of the background push.
const DEFAULT_UPKEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Checkpoint the local log once it passes this size. Turso Sync turns the
/// engine's automatic checkpoints off.
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;

/// With a copy, answer changes only after they reach it and keep it current.
pub(super) fn answer_after_copy(router: Router, copy: Option<SyncCopy>) -> Router {
    let Some(copy) = copy else {
        return router;
    };
    spawn_upkeep(copy.clone());
    router.layer(axum::middleware::from_fn_with_state(
        copy,
        push_before_answering,
    ))
}

/// Push every change committed while handling a request before answering it.
async fn push_before_answering(
    State(copy): State<SyncCopy>,
    request: Request,
    next: Next,
) -> Response {
    let reads_only = reads_only(request.method(), request.uri().path());
    let response = next.run(request).await;
    if reads_only {
        return response;
    }
    match copy.push_committed_within(ANSWER_PUSH_BUDGET).await {
        Ok(()) => response,
        Err(error) => {
            tracing::error!(%error, "turso.sync: change not copied off this server; not confirming it");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Not confirmed: the change could not be copied to the app's remote database. \
                 It may still be applied here; read it back before retrying.",
            )
                .into_response()
        }
    }
}

/// Requests that cannot change anything: reads of Temper's own routes. A
/// GET to an app's HTTP endpoint or webhook can run actions, so it waits too.
fn reads_only(method: &Method, path: &str) -> bool {
    let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let own_route = ["/tdata", "/observe", "/api/", "/genesis", "/static/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
        || matches!(path, "/healthz" | "/version" | "/temper-client.js");
    safe && own_route
}

/// Push changes no request waited for and checkpoint the local log.
fn spawn_upkeep(copy: SyncCopy) {
    let interval = std::env::var("TEMPER_TURSO_SYNC_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_UPKEEP_INTERVAL);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(error) = copy.push_committed().await {
                tracing::warn!(%error, "turso.sync background push failed");
                continue;
            }
            if let Err(error) = copy.checkpoint_if_wal_over(MAX_WAL_BYTES).await {
                tracing::warn!(%error, "turso.sync checkpoint failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reads_of_temper_routes_skip_the_push() {
        assert!(reads_only(&Method::GET, "/tdata/Notes('a')"));
        assert!(reads_only(&Method::HEAD, "/healthz"));
        assert!(!reads_only(&Method::POST, "/tdata/Notes('a')/Ns.Write"));
        assert!(!reads_only(&Method::GET, "/webhooks/default/callback"));
        assert!(!reads_only(&Method::GET, "/oauth/callback"));
    }
}
