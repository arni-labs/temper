//! Rolling back from a release that writes in MVCC mode: this release opens
//! the file that release left, in WAL mode with every row, and pushes the
//! rows that release had not.

use std::path::{Path, PathBuf};

use super::support::*;

/// Set for the child process that writes as the newer release: the file it
/// writes, the remote, and how it stops (`clean` or `crash`).
const NEWER_RELEASE: &str = "TEMPER_TEST_NEWER_RELEASE";
const NEWER_RELEASE_URL: &str = "TEMPER_TEST_NEWER_RELEASE_URL";
const NEWER_RELEASE_STOP: &str = "TEMPER_TEST_NEWER_RELEASE_STOP";

/// Rows the newer release writes in MVCC mode, and how many of them it pushes.
const MVCC_ROWS: i64 = 20;
const MVCC_PUSHED: i64 = 10;

#[tokio::test]
async fn a_file_a_newer_release_closed_in_mvcc_mode_opens_with_every_row() {
    rolls_back("clean").await;
}

#[tokio::test]
async fn a_file_a_newer_release_crashed_in_mvcc_mode_opens_with_every_row() {
    rolls_back("crash").await;
}

/// The newer release, run as a child process by the checks above. Without
/// its variables set it does nothing.
#[tokio::test]
async fn newer_release() {
    let Ok(path) = std::env::var(NEWER_RELEASE) else {
        return;
    };
    let url = std::env::var(NEWER_RELEASE_URL).expect("remote");
    let crash = std::env::var(NEWER_RELEASE_STOP).expect("stop") == "crash";
    write_in_mvcc_mode(Path::new(&path), &url, crash).await;
}

async fn rolls_back(stop: &str) {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    for i in 0..3 {
        write(&a, "before", i).await;
    }
    push(&a).await;
    drop(a);

    let newer = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["rollback::newer_release", "--exact", "--nocapture"])
        .env(NEWER_RELEASE, &a_path)
        .env(NEWER_RELEASE_URL, &server.url)
        .env(NEWER_RELEASE_STOP, stop)
        .status()
        .expect("run the newer release");
    assert!(newer.success(), "the newer release wrote its rows");
    assert!(in_mvcc_mode(&a_path), "the newer release left MVCC mode on");

    let a = server.open(&a_path).await;
    assert_eq!(count(&a, "before").await, 3);
    write(&a, "after", 0).await;
    push(&a).await;
    drop(a);
    assert!(!in_mvcc_mode(&a_path), "the file is back in WAL mode");
    assert_eq!(mvcc_rows(&a_path).await, MVCC_ROWS);

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "before").await, 3);
    assert_eq!(count(&b, "after").await, 1);
    drop(b);
    assert_eq!(
        mvcc_rows(&b_path).await,
        MVCC_ROWS,
        "the rows the newer release had not pushed reached the remote"
    );
}

/// What a release on Turso's concurrent writes does with a synced file:
/// switch it to MVCC mode, number its changes above every change pushed so
/// far, and write in concurrent transactions, pushing some.
async fn write_in_mvcc_mode(path: &Path, url: &str, crash: bool) {
    // The sync engine's HTTP client needs the one crypto provider chosen.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let local = path.to_str().expect("utf-8");
    let open = || async {
        turso::sync::Builder::new_remote(local)
            .with_remote_url(url)
            .with_auth_token(TOKEN)
            .with_client_name("temper")
            .bootstrap_if_empty(true)
            .build()
            .await
            .expect("open with sync")
    };
    {
        let db = open().await;
        let conn = db.connect().await.expect("connect");
        conn.execute(
            "CREATE TABLE mvcc_rows (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
            (),
        )
        .await
        .expect("table");
        db.push().await.expect("push the table");
    }
    // Turso refuses the switch on a connection that captures changes.
    {
        let plain = turso::Builder::new_local(local)
            .build()
            .await
            .expect("plain open");
        let conn = plain.connect().expect("connect");
        let mut rows = conn
            .query("PRAGMA journal_mode = 'mvcc'", ())
            .await
            .expect("switch");
        let row = rows.next().await.expect("row").expect("journal mode");
        assert_eq!(row.get::<String>(0).expect("text"), "mvcc");
    }
    let db = open().await;
    let conn = db.connect().await.expect("connect");
    // After the switch turso 0.8.3 numbers changes from 1 again, below the
    // changes already pushed, which a push skips.
    let pushed = pushed_hint(path);
    conn.execute(
        &format!(
            "INSERT INTO turso_cdc (change_id, change_time, change_type) VALUES \
             (MAX({pushed}, (SELECT COALESCE(MAX(change_id), 0) FROM turso_cdc)) + 1, 0, 2)"
        ),
        (),
    )
    .await
    .expect("number changes above those pushed");
    for n in 0..MVCC_ROWS {
        conn.execute("BEGIN CONCURRENT", ()).await.expect("begin");
        conn.execute("INSERT INTO mvcc_rows (n) VALUES (?1)", turso::params![n])
            .await
            .expect("insert");
        conn.execute("COMMIT", ()).await.expect("commit");
        if n + 1 == MVCC_PUSHED {
            db.push().await.expect("push");
        }
    }
    if crash {
        // Stop without closing anything.
        std::process::exit(0);
    }
}

/// The change number the engine last pushed, from its sync metadata.
fn pushed_hint(path: &Path) -> i64 {
    let info = std::fs::read_to_string(PathBuf::from(format!("{}-info", path.display())))
        .expect("sync metadata");
    serde_json::from_str::<serde_json::Value>(&info).expect("json")["last_pushed_change_id_hint"]
        .as_i64()
        .unwrap_or(0)
}

/// A database file's header holds 255 at bytes 18 and 19 in MVCC mode.
fn in_mvcc_mode(path: &Path) -> bool {
    std::fs::read(path).expect("database file")[18..20].contains(&255)
}

async fn mvcc_rows(path: &Path) -> i64 {
    let db = turso::Builder::new_local(path.to_str().expect("utf-8"))
        .build()
        .await
        .expect("open");
    let conn = db.connect().expect("connect");
    let mut rows = conn
        .query("SELECT COUNT(*) FROM mvcc_rows", ())
        .await
        .expect("count");
    let row = rows.next().await.expect("row").expect("a count");
    row.get::<i64>(0).expect("integer")
}
