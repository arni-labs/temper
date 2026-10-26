//! Answers wait for the database's Turso Cloud copy (Turso Sync).
//!
//! With `TURSO_SYNC_URL` set, the database stays a local file and every
//! request that can change it is answered only after its changes reach the
//! remote copy. Requests that finish together share one push. A background
//! loop also pushes changes no request waits for (timers, reactions), trims
//! the change log the remote has confirmed, keeps the local write-ahead log
//! short, and checks a file the start could not check. While writes cannot
//! be confirmed, requests that can change data are refused before they run
//! and reads are served as usual.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_store_turso::{CopyState, SyncCopy};

/// How long a request that can change data may take, its push included,
/// before its change is reported as not confirmed.
const ANSWER_BUDGET: Duration = Duration::from_secs(10);

/// The least time left for the push of a slow request.
const MIN_PUSH_BUDGET: Duration = Duration::from_secs(1);

/// Default period of the background push.
const DEFAULT_UPKEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The 1 s ceiling lets the background push run once each tick. With a
/// request in flight the trim stops after its 250 ms floor and current batch;
/// idle ticks use the whole second to drain the backlog.
const TRIM_BUDGET: Duration = Duration::from_secs(1);

/// Checkpoint the local log once it passes this size. Turso Sync turns the
/// engine's automatic checkpoints off.
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;

/// Push attempts timing out in a row before the server exits. The sync
/// engine sends its requests one at a time, so one the remote never answers
/// holds every later push until its connection fails; the supervisor's
/// restart starts a fresh one, and the start checks the file again.
const TIMEOUTS_BEFORE_RESTART: u32 = 6;

/// With a copy, answer changes only after they reach it and keep it current.
pub(super) fn answer_after_copy(router: Router, copy: Option<SyncCopy>) -> Router {
    let Some(copy) = copy else {
        return router;
    };
    let in_flight = Arc::new(AtomicUsize::new(0));
    spawn_upkeep(copy.clone(), in_flight.clone());
    router.layer(axum::middleware::from_fn_with_state(
        (copy, in_flight),
        push_before_answering,
    ))
}

/// Refuse a request that can change data while writes cannot be confirmed;
/// otherwise run it and push what it committed before answering.
async fn push_before_answering(
    State((copy, in_flight)): State<(SyncCopy, Arc<AtomicUsize>)>,
    request: Request,
    next: Next,
) -> Response {
    if reads_only(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    // Count through the answer's push, including cancellation and refusals.
    in_flight.fetch_add(1, Ordering::AcqRel);
    let _request = InFlight(in_flight);
    if let Some(refusal) = refusal(&copy.state()) {
        return refusal;
    }
    let started = Instant::now();
    let response = next.run(request).await;
    let budget = ANSWER_BUDGET
        .saturating_sub(started.elapsed())
        .max(MIN_PUSH_BUDGET);
    match copy.push_committed_within(budget).await {
        Ok(()) => response,
        // The request ran, so whatever the copy's state now, the change may
        // have been saved here: never answer as if nothing happened.
        Err(error) => {
            tracing::error!(%error, "turso.sync: change not copied off this server; not confirming it");
            unavailable(
                "NotConfirmed",
                "Not confirmed: the change could not be copied to this app's database copy. \
                 It may still have been saved here; read it back before trying again.",
            )
        }
    }
}

/// Drop also handles a cancelled request, so upkeep never sees a stale count.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The answer to a request that can change data in `state`, if it is refused.
fn refusal(state: &CopyState) -> Option<Response> {
    let (code, message) = match state {
        CopyState::Copying => return None,
        CopyState::Unchecked => (
            "SavingPaused",
            "Saving is paused until this app's database copy can be reached. Reading still works.",
        ),
        CopyState::Behind => (
            "SavingPaused",
            "Saving is paused while this app restores its database from its copy. \
             Try again in a minute.",
        ),
        CopyState::Stopped(_) => (
            "SavingStopped",
            "Saving is stopped: this app's database copy is missing a change, so nothing new \
             can be confirmed until an operator rebuilds the copy. Reading still works.",
        ),
    };
    Some(unavailable(code, message))
}

fn unavailable(code: &str, message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

/// Requests that cannot change anything: reads of Temper's own routes. A
/// read of an app's HTTP endpoint or webhook can run actions, so it waits too.
fn reads_only(method: &Method, path: &str) -> bool {
    let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    let own_route = ["/tdata", "/observe", "/api", "/genesis"]
        .iter()
        .any(|prefix| {
            path.strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
        || matches!(
            path,
            "/healthz" | "/version" | "/temper-client.js" | "/static/temper-client.js"
        );
    safe && own_route
}

/// Push changes no request waited for, check a file the start could not
/// check, checkpoint the local log, and restart when only a restart helps.
fn spawn_upkeep(copy: SyncCopy, in_flight: Arc<AtomicUsize>) {
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
            copy.record_state();
            if let Err(error) = copy.recheck().await {
                tracing::debug!(%error, "turso.sync remote still unreachable");
            }
            if copy.state() == CopyState::Behind {
                tracing::error!(
                    "turso.sync the file is behind its remote copy; exiting so the restart restores it"
                );
                std::process::exit(75);
            }
            if copy.state() != CopyState::Copying {
                continue;
            }
            if let Err(error) = copy.push_committed().await {
                tracing::warn!(%error, "turso.sync background push failed");
            }
            if copy.pushes_timed_out_in_a_row() >= TIMEOUTS_BEFORE_RESTART {
                tracing::error!(
                    timeouts = copy.pushes_timed_out_in_a_row(),
                    "turso.sync pushes keep timing out; exiting so the restart opens a fresh connection"
                );
                std::process::exit(75);
            }
            if let Err(error) = copy
                .trim_pushed(TRIM_BUDGET, || in_flight.load(Ordering::Acquire) > 0)
                .await
            {
                tracing::warn!(%error, "turso.sync could not trim the change log; the next tick will");
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
        assert!(reads_only(&Method::GET, "/tdata"));
        assert!(reads_only(&Method::HEAD, "/healthz"));
        assert!(reads_only(&Method::GET, "/static/temper-client.js"));
        assert!(!reads_only(&Method::POST, "/tdata/Notes('a')/Ns.Write"));
        assert!(!reads_only(&Method::GET, "/webhooks/default/callback"));
        assert!(!reads_only(&Method::GET, "/oauth/callback"));
        assert!(!reads_only(&Method::GET, "/static/app-page"));
        assert!(!reads_only(&Method::GET, "/observer"));
        assert!(!reads_only(&Method::GET, "/genesisx"));
    }

    #[test]
    fn only_a_copying_database_runs_requests_that_can_change_data() {
        assert!(refusal(&CopyState::Copying).is_none());
        for state in [
            CopyState::Unchecked,
            CopyState::Behind,
            CopyState::Stopped("refused".to_string()),
        ] {
            let refused = refusal(&state).expect("refused");
            assert_eq!(
                refused.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{state:?}"
            );
        }
    }
}
