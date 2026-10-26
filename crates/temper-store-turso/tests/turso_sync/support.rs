//! Helpers shared by the Turso Sync tests: a local sync server, a machine's
//! data folder, and writes to count.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_turso::TursoEventStore;

pub const TOKEN: &str = "local-sync-server-ignores-tokens";

pub struct SyncServer {
    child: Child,
    pub url: String,
    _dir: tempfile::TempDir,
}

impl SyncServer {
    pub fn start() -> Option<Self> {
        let Some(bin) = std::env::var_os("TEMPER_TEST_TURSODB") else {
            assert!(
                std::env::var_os("CI").is_none(),
                "TEMPER_TEST_TURSODB must name a tursodb binary in CI"
            );
            eprintln!("skipped: TEMPER_TEST_TURSODB is not set");
            return None;
        };
        // The store's and the sync engine's logs, and the server's below, show
        // in the output of a test that fails or times out.
        let _ = tracing_subscriber::fmt()
            .with_env_filter("temper_store_turso=debug,turso_sync_engine=info")
            .with_writer(std::io::stderr)
            .try_init();
        let dir = tempfile::tempdir().expect("server dir");
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let child = Command::new(bin)
            .arg(dir.path().join("remote.db"))
            .args(["--sync-server", &format!("127.0.0.1:{port}")])
            .env("RUST_LOG", "info")
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
        let started = Instant::now();
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

    pub async fn open(&self, path: &Path) -> TursoEventStore {
        TursoEventStore::new_synced(path.to_str().expect("utf-8 path"), &self.url, TOKEN)
            .await
            .expect("open synced store")
    }
}

impl SyncServer {
    /// Run one statement on the remote database directly.
    pub fn remote_sql(&self, sql: &str) {
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

pub fn machine() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("machine dir");
    let path = dir.path().join("app.db");
    (dir, path)
}

pub fn event(n: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Written".to_string(),
        payload: serde_json::json!({ "n": n, "body": "x".repeat(1500) }),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "turso-sync-test".to_string(),
        },
    }
}

pub async fn write(store: &TursoEventStore, entity: &str, after: u64) {
    store
        .append(
            &format!("default:Note:{entity}"),
            after,
            &[event(after + 1)],
        )
        .await
        .expect("append");
}

pub async fn count(store: &TursoEventStore, entity: &str) -> usize {
    store
        .read_events(&format!("default:Note:{entity}"), 0)
        .await
        .expect("read events")
        .len()
}

pub async fn push(store: &TursoEventStore) {
    store
        .sync_copy()
        .expect("synced store has a copy")
        .push_committed()
        .await
        .expect("push");
}

/// Confirmed change log rows for the long-log tests: more than half the
/// Factory's 532,000 changes.
pub const BIG_LOG: i64 = 300_000;

/// Open a store at `path` whose change log holds `rows` changes the remote
/// has confirmed, below one pushed write.
///
/// A push carries only changes numbered above the remote's record of this
/// file's last push, so every row at or below it is the confirmed part of
/// the log. Pushing 300,000 changes for real is one request of one
/// statement per change, far past a push's 10 s limit, so the confirmed
/// part is written into the log directly, numbered below the first real
/// change. Most rows hold a 100-byte image; every thousandth holds 30 KB,
/// the size of the Factory's catalog images, which spill onto overflow
/// pages. Writing the log costs about its size: with 1 KB rows and every
/// hundredth at 30 KB (about 390 MB) the insert alone took 93 s on a CI
/// runner, past the 120 s a sync test may run once two run side by side.
pub async fn confirmed_log(
    server: &SyncServer,
    path: &Path,
    rows: i64,
) -> (TursoEventStore, temper_store_turso::SyncCopy) {
    let store = server.open(path).await;
    let copy = store.sync_copy().expect("copy").clone();
    write(&store, "pushed", 0).await;
    push(&store).await;
    let before = copy.change_log_rows().await.expect("rows");
    let added = copy
        .execute_local(&format!(
            "INSERT INTO turso_cdc \
             (change_id, change_time, change_type, table_name, id, before, after, updates) \
             SELECT value - {rows}, 0, 1, 'events', value, NULL, \
             CASE WHEN value % 1000 = 0 THEN randomblob(30000) ELSE randomblob(100) END, NULL \
             FROM generate_series(1, {rows})"
        ))
        .await
        .expect("fill the change log");
    assert_eq!(added, rows as u64);
    copy.checkpoint_if_wal_over(0).await.expect("checkpoint");
    assert_eq!(copy.change_log_rows().await.expect("rows"), before + rows);
    (store, copy)
}

pub fn copy_files(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
        }
    }
}

pub fn remove_files(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            std::fs::remove_file(entry.path()).expect("remove");
        }
    }
}

/// Forwards to the sync server until closed, then swallows every byte, as a
/// remote that stops answering.
pub async fn valve(upstream: &str) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let open = std::sync::Arc::new(AtomicBool::new(true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let upstream = upstream.trim_start_matches("http://").to_string();
    let flag = open.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
                continue;
            };
            let (mut client_read, mut client_write) = client.into_split();
            let (mut server_read, mut server_write) = server.into_split();
            let up = flag.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = client_read.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if up.load(Ordering::Acquire)
                        && server_write.write_all(&buf[..n]).await.is_err()
                    {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = server_read.read(&mut buf).await {
                    if n == 0 || client_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (url, open)
}

/// Forwards to the sync server while on; while off, closes every new
/// connection at once, as a remote that cannot be reached.
pub async fn switch(upstream: &str) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    let on = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let upstream = upstream.trim_start_matches("http://").to_string();
    let flag = on.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            if !flag.load(Ordering::Acquire) {
                continue;
            }
            let upstream = upstream.clone();
            tokio::spawn(async move {
                if let Ok(mut server) = tokio::net::TcpStream::connect(&upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                }
            });
        }
    });
    (url, on)
}
