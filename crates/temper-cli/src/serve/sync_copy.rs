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

use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_server::odata::IDEMPOTENCY_KEY;
use temper_store_turso::{CopyState, SyncCopy, TursoEventStore};

/// How long a request that can change data may take, its push included,
/// before its change is reported as not confirmed.
const ANSWER_BUDGET: Duration = Duration::from_secs(10);

/// The least time left for the push of a slow request.
const MIN_PUSH_BUDGET: Duration = Duration::from_secs(1);

/// Default period of the background push.
const DEFAULT_UPKEEP_INTERVAL: Duration = Duration::from_secs(1);

/// The 1 s ceiling lets the background push and the checkpoint run once each
/// tick. Requests need no share of it: a write that waits goes before the
/// trim's next batch (`TursoEventStore::trim_change_log`).
const TRIM_BUDGET: Duration = Duration::from_secs(1);

/// Checkpoint the local log once it passes this size. Turso Sync turns the
/// engine's automatic checkpoints off.
const MAX_WAL_BYTES: u64 = 16 * 1024 * 1024;

/// Push attempts timing out in a row before the server exits. The sync
/// engine sends its requests one at a time, so one the remote never answers
/// holds every later push until its connection fails; the supervisor's
/// restart starts a fresh one, and the start checks the file again.
const TIMEOUTS_BEFORE_RESTART: u32 = 6;

/// The server's router: `routes` with, when the store's database has a copy,
/// changes answered only after they reach it, and one span per request
/// around all of it. The span goes on last, so a request's push, and the
/// answer it ends in, are part of that request's trace.
pub(super) fn serve_router(routes: Router, store: Option<TursoEventStore>) -> Router {
    answer_after_copy(temper_server::with_request_span(routes), store)
}

/// With a store whose database has a copy, answer changes only after they
/// reach it and keep it current.
fn answer_after_copy(routes: Router, store: Option<TursoEventStore>) -> Router {
    let Some((store, copy)) =
        store.and_then(|store| store.sync_copy().cloned().map(|copy| (store, copy)))
    else {
        return routes;
    };
    spawn_upkeep(store, copy.clone());
    routes.layer(axum::middleware::from_fn_with_state(
        copy,
        push_before_answering,
    ))
}

/// Refuse a request that can change data while writes cannot be confirmed;
/// otherwise run it and push what it committed before answering.
async fn push_before_answering(
    State(copy): State<SyncCopy>,
    request: Request,
    next: Next,
) -> Response {
    if reads_only(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
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
            not_confirmed(response.headers().get(IDEMPOTENCY_KEY))
        }
    }
}

/// The answer to a request that ran but whose change could not be copied
/// off this server. It names the key the request ran with, when it has one,
/// so the caller can send it again as the same write.
fn not_confirmed(key: Option<&HeaderValue>) -> Response {
    let key = key.and_then(|value| Some((value, value.to_str().ok()?)));
    let mut error = serde_json::json!({
        "code": "NotConfirmed",
        "message": "Not confirmed: the change could not be copied to this app's database copy. \
                    It may still have been saved here; read it back before trying again.",
    });
    if let Some((_, text)) = key {
        error["idempotencyKey"] = text.into();
    }
    let mut answer = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({ "error": error })),
    )
        .into_response();
    if let Some((value, _)) = key {
        answer.headers_mut().insert(IDEMPOTENCY_KEY, value.clone());
    }
    answer
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
/// check, trim the change log, checkpoint the local log, and restart when
/// only a restart helps.
fn spawn_upkeep(store: TursoEventStore, copy: SyncCopy) {
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
            if let Err(error) = store.trim_change_log(TRIM_BUDGET).await {
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

    #[tokio::test]
    async fn a_not_confirmed_answer_names_the_key_its_write_ran_with() {
        let key = HeaderValue::from_static("write-key-1");
        let answer = not_confirmed(Some(&key));
        assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(answer.headers().get(IDEMPOTENCY_KEY), Some(&key));
        let body = axum::body::to_bytes(answer.into_body(), 64 * 1024)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(body["error"]["code"], "NotConfirmed");
        assert_eq!(body["error"]["idempotencyKey"], "write-key-1");

        // A request that came without one (an app's own route) still gets
        // the answer, without a key.
        let unkeyed = not_confirmed(None);
        assert!(unkeyed.headers().get(IDEMPOTENCY_KEY).is_none());
    }

    /// A span's name, its parent's name, and the fields given a value.
    #[derive(Debug)]
    struct SpanSeen {
        name: String,
        parent: Option<String>,
        fields: std::collections::BTreeSet<String>,
    }

    /// Every span made, in order, and the fields recorded on it later.
    #[derive(Clone, Default)]
    struct Spans(std::sync::Arc<std::sync::Mutex<Vec<SpanSeen>>>);

    /// Collects the names of the fields a span is given.
    struct FieldNames<'a>(&'a mut std::collections::BTreeSet<String>);

    impl tracing::field::Visit for FieldNames<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, _: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_string());
        }
    }

    impl<S> tracing_subscriber::Layer<S> for Spans
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            id: &tracing::span::Id,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span = ctx.span(id).expect("a new span is registered");
            let mut fields = std::collections::BTreeSet::new();
            attrs.record(&mut FieldNames(&mut fields));
            let mut seen = self.0.lock().expect("spans");
            span.extensions_mut().insert(seen.len());
            seen.push(SpanSeen {
                name: span.name().to_string(),
                parent: span.parent().map(|parent| parent.name().to_string()),
                fields,
            });
        }

        fn on_record(
            &self,
            id: &tracing::span::Id,
            values: &tracing::span::Record<'_>,
            ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let span = ctx.span(id).expect("a recorded span is registered");
            let Some(&at) = span.extensions().get::<usize>() else {
                return;
            };
            let mut seen = self.0.lock().expect("spans");
            values.record(&mut FieldNames(&mut seen[at].fields));
        }
    }

    /// Turso's local sync server (`tursodb --sync-server`), the remote the
    /// store's turso_sync tests use.
    struct SyncServer {
        child: std::process::Child,
        url: String,
        _dir: tempfile::TempDir,
    }

    impl SyncServer {
        async fn start() -> Option<Self> {
            let bin = temper_test_resource::env_var("TEMPER_TEST_TURSODB")?;
            let dir = tempfile::tempdir().expect("server dir");
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|listener| listener.local_addr())
                .expect("free port")
                .port();
            let child = std::process::Command::new(bin)
                .arg(dir.path().join("remote.db"))
                .args(["--sync-server", &format!("127.0.0.1:{port}")])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("start tursodb --sync-server");
            let server = Self {
                child,
                url: format!("http://127.0.0.1:{port}"),
                _dir: dir,
            };
            // Ready once it answers a statement, not merely once its port is bound.
            let started = Instant::now();
            let client = reqwest::Client::new();
            let select = serde_json::json!({
                "requests": [{ "type": "execute", "stmt": { "sql": "SELECT 1" } }]
            });
            loop {
                let answer = client
                    .post(format!("{}/v2/pipeline", server.url))
                    .json(&select)
                    .send()
                    .await;
                if answer.is_ok_and(|answer| answer.status().is_success()) {
                    return Some(server);
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "sync server did not start"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    impl Drop for SyncServer {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// A write's push is part of the write's trace: it runs under the
    /// request's span, so a slow answer shows its push, and says how long it
    /// queued behind another push.
    #[tokio::test]
    async fn a_writes_push_is_inside_its_request_span() {
        use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
        use tracing_subscriber::layer::SubscriberExt as _;

        let Some(server) = SyncServer::start().await else {
            return;
        };
        let dir = tempfile::tempdir().expect("data dir");
        let local = format!("file:{}", dir.path().join("app.db").display());
        let store = TursoEventStore::new_synced(&local, &server.url, "the-local-server-ignores-it")
            .await
            .expect("open the synced store");
        let writer = store.clone();
        let routes = Router::new().route(
            "/write",
            axum::routing::post(move || async move {
                let event = PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "Written".to_string(),
                    payload: serde_json::json!({ "n": 1 }),
                    metadata: EventMetadata {
                        event_id: uuid::Uuid::new_v4(),
                        causation_id: uuid::Uuid::new_v4(),
                        correlation_id: uuid::Uuid::new_v4(),
                        timestamp: chrono::Utc::now(),
                        actor_id: "serve-test".to_string(),
                    },
                };
                writer
                    .append("default:Note:a", 0, &[event])
                    .await
                    .expect("append");
                StatusCode::NO_CONTENT
            }),
        );
        // The test's runtime has one thread, so the server's tasks report here.
        let spans = Spans::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(spans.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let router = serve_router(routes, Some(store));
        tokio::spawn(async move { axum::serve(listener, router).await });

        let answer = reqwest::Client::new()
            .post(format!("http://{addr}/write"))
            .send()
            .await
            .expect("an answer");
        assert_eq!(
            answer.status(),
            StatusCode::NO_CONTENT,
            "the write is confirmed"
        );

        let seen = spans.0.lock().expect("spans");
        let push = seen
            .iter()
            .find(|span| {
                span.name == "turso.sync.push" && span.parent.as_deref() == Some("http.request")
            })
            .unwrap_or_else(|| panic!("the write's push is inside its request span: {seen:?}"));
        for field in ["waited_ms", "carried"] {
            assert!(
                push.fields.contains(field),
                "the push records {field}: {push:?}"
            );
        }
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
