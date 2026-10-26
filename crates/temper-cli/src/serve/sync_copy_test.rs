use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::routing::post;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_turso::TursoEventStore;

use super::*;

#[tokio::test(start_paused = true)]
async fn a_failing_push_is_retried_later_each_time_up_to_the_cap() {
    let interval = Duration::from_secs(1);
    let mut retry = Retry::new(interval);
    assert!(retry.due(), "the first push goes at once");
    let mut waits = Vec::new();
    for _ in 0..8 {
        waits.push(retry.failed().as_secs());
        assert!(!retry.due(), "not again before the wait");
        tokio::time::advance(Duration::from_secs(*waits.last().unwrap())).await;
        assert!(retry.due(), "due once the wait has passed");
    }
    assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
    retry.succeeded();
    assert!(retry.due());
    assert_eq!(retry.failed(), interval, "a success starts the waits over");
}

const TOKEN: &str = "local-sync-server-ignores-tokens";

/// Turso's local sync server, from `TEMPER_TEST_TURSODB` (tursodb 0.7.2, the
/// version the `turso` crate pins). Without it the tests are skipped locally
/// and fail in CI.
struct SyncServer {
    child: Child,
    url: String,
    _dir: tempfile::TempDir,
}

impl SyncServer {
    fn start() -> Option<Self> {
        let Some(bin) = std::env::var_os("TEMPER_TEST_TURSODB") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "TEMPER_TEST_TURSODB must name a tursodb binary in CI"
            );
            eprintln!("skipped: TEMPER_TEST_TURSODB is not set");
            return None;
        };
        let dir = tempfile::tempdir().expect("server dir");
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let child = Command::new(bin)
            .arg(dir.path().join("remote.db"))
            .args(["--sync-server", &format!("127.0.0.1:{port}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start tursodb --sync-server");
        let server = Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            _dir: dir,
        };
        // Ready once it answers a statement, not merely once its port is bound.
        let started = std::time::Instant::now();
        while server
            .post("SELECT 1")
            .is_none_or(|answer| !answer.starts_with("HTTP/1.1 200"))
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "sync server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(server)
    }

    fn post(&self, sql: &str) -> Option<String> {
        let addr = self.url.trim_start_matches("http://");
        let body = serde_json::json!({
            "requests": [{ "type": "execute", "stmt": { "sql": sql } }]
        })
        .to_string();
        let mut stream = TcpStream::connect(addr).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .ok()?;
        write!(
            stream,
            "POST /v2/pipeline HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .ok()?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer).ok()?;
        Some(answer)
    }

    fn remote_sql(&self, sql: &str) {
        let answer = self.post(sql).expect("sync server answers");
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(!answer.contains("\"type\":\"error\""), "{answer}");
    }
}

impl Drop for SyncServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn event(n: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Written".to_string(),
        payload: serde_json::json!({ "n": n }),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "turso-backup-test".to_string(),
        },
    }
}

/// A server whose one route commits a write to `store`, with the backup the
/// way `temper serve` runs it.
async fn serve(store: Arc<TursoEventStore>) -> String {
    let copy = store.sync_copy().cloned();
    let writes = Router::new().route(
        "/write/{after}",
        post(
            move |axum::extract::Path(after): axum::extract::Path<u64>| {
                let store = store.clone();
                async move {
                    match store
                        .append("default:Note:n1", after, &[event(after + 1)])
                        .await
                    {
                        Ok(_) => StatusCode::OK,
                        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
                    }
                }
            },
        ),
    );
    let app = with_backup(writes, copy);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    url
}

async fn write(url: &str, after: u64) -> StatusCode {
    reqwest::Client::new()
        .post(format!("{url}/write/{after}"))
        .send()
        .await
        .expect("answered")
        .status()
}

#[tokio::test]
async fn a_write_is_confirmed_while_the_remote_is_unreachable() {
    let Some(remote) = SyncServer::start() else {
        return;
    };
    let dir = tempfile::tempdir().expect("data dir");
    let path = dir.path().join("app.db");
    let store = TursoEventStore::new_synced(path.to_str().expect("utf-8"), &remote.url, TOKEN)
        .await
        .expect("open synced store");
    let url = serve(Arc::new(store)).await;
    assert_eq!(write(&url, 0).await, StatusCode::OK);

    drop(remote);
    let started = std::time::Instant::now();
    assert_eq!(
        write(&url, 1).await,
        StatusCode::OK,
        "the commit on the drive is the answer"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the answer does not wait on the remote: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_write_is_confirmed_while_the_backup_is_stopped() {
    let Some(remote) = SyncServer::start() else {
        return;
    };
    let dir = tempfile::tempdir().expect("data dir");
    let path = dir.path().join("app.db");
    let store = Arc::new(
        TursoEventStore::new_synced(path.to_str().expect("utf-8"), &remote.url, TOKEN)
            .await
            .expect("open synced store"),
    );
    let copy = store.sync_copy().expect("copy").clone();
    store
        .append("default:Note:n1", 0, &[event(1)])
        .await
        .expect("append");
    copy.push_committed().await.expect("push");
    // The remote now refuses the next event and commits the rest of its push.
    remote.remote_sql("DROP TABLE events");
    store
        .append("default:Note:n1", 1, &[event(2)])
        .await
        .expect("append");
    copy.push_committed()
        .await
        .expect_err("the remote refused the event");
    assert!(
        matches!(copy.state(), CopyState::Stopped(_)),
        "{:?}",
        copy.state()
    );

    let url = serve(store).await;
    assert_eq!(
        write(&url, 2).await,
        StatusCode::OK,
        "a stopped backup is no reason to refuse a write"
    );
}
