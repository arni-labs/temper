//! Starting a synced database: without the remote, from an older copy, and
//! from a database that was never synced.

use std::path::PathBuf;

use super::support::*;
use temper_store_turso::{CopyState, TursoEventStore};

#[tokio::test]
async fn a_start_without_the_remote_serves_reads_and_confirms_writes_once_it_answers() {
    // Rita, 17 Oct: "Read only". Whether the file is behind the remote can be
    // told only by asking the remote, so until then nothing is pushed.
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, on) = switch(&server.url).await;
    let (_a_dir, a_path) = machine();
    let a = TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("open");
    write(&a, "u1", 0).await;
    push(&a).await;
    drop(a);

    on.store(false, std::sync::atomic::Ordering::Release);
    let a = TursoEventStore::new_synced(a_path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("a start without the remote still serves");
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    assert_eq!(count(&a, "u1").await, 1, "reads are served");
    copy.push_committed()
        .await
        .expect_err("nothing is confirmed unchecked");
    copy.recheck().await.expect_err("still unreachable");

    on.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(copy.recheck().await.expect("recheck"), CopyState::Copying);
    write(&a, "u1", 1).await;
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "u1").await, 2);
}

#[tokio::test]
async fn an_older_copy_opened_without_the_remote_is_restored_once_it_answers() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, on) = switch(&server.url).await;
    let open = |path: PathBuf| {
        let url = url.clone();
        async move {
            TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
                .await
                .expect("open")
        }
    };
    let (a_dir, a_path) = machine();
    let older = tempfile::tempdir().expect("snapshot dir");
    let a = open(a_path.clone()).await;
    write(&a, "old", 0).await;
    push(&a).await;
    drop(a);
    copy_files(a_dir.path(), older.path());
    let a = open(a_path.clone()).await;
    for i in 0..5 {
        write(&a, "later", i).await;
    }
    push(&a).await;
    drop(a);
    remove_files(a_dir.path());
    copy_files(older.path(), a_dir.path());

    on.store(false, std::sync::atomic::Ordering::Release);
    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    // Work the server does on its own still commits locally while unchecked,
    // here more changes than the remote is ahead by, so the file's newest
    // change number passes the remote's: only the number at open tells.
    for i in 0..10 {
        write(&a, "meanwhile", i).await;
    }
    // A restart during the outage keeps the number from the first start.
    drop((a, copy));
    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    on.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(copy.recheck().await.expect("recheck"), CopyState::Behind);
    copy.push_committed()
        .await
        .expect_err("a file behind confirms nothing");
    drop((a, copy));

    // The restart the server makes restores the file from the remote.
    let a = open(a_path).await;
    assert_eq!(a.sync_copy().expect("copy").state(), CopyState::Copying);
    assert_eq!(count(&a, "later").await, 5);
    write(&a, "after", 0).await;
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "after").await, 1);
}

#[tokio::test]
async fn a_database_written_before_sync_is_adopted_onto_an_empty_remote() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (a_dir, a_path) = machine();
    // A file from before primary keys: specs keyed by UNIQUE only.
    {
        let db = turso::Builder::new_local(a_path.to_str().expect("utf-8"))
            .build()
            .await
            .expect("old database");
        db.connect()
            .expect("connect")
            .execute(
                "CREATE TABLE specs (tenant TEXT NOT NULL, entity_type TEXT NOT NULL, \
                 ioa_source TEXT NOT NULL, csdl_xml TEXT, version INTEGER NOT NULL DEFAULT 1, \
                 verified INTEGER NOT NULL DEFAULT 0, verification_status TEXT NOT NULL DEFAULT 'pending', \
                 levels_passed INTEGER, levels_total INTEGER, verification_result TEXT, \
                 updated_at TEXT NOT NULL DEFAULT (datetime('now')), UNIQUE(tenant, entity_type))",
                (),
            )
            .await
            .expect("old specs table");
    }
    let plain = TursoEventStore::new(&format!("file:{}", a_path.display()), None)
        .await
        .expect("plain store");
    for i in 0..3 {
        write(&plain, "kept", i).await;
    }
    plain
        .upsert_specs_and_commit("default", &[("Note", "v1", "<csdl/>", "v1")], None, "notes")
        .await
        .expect("spec");
    drop(plain);

    let unreachable = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        format!("http://{}", listener.local_addr().expect("addr"))
    };
    let db = a_path.to_str().expect("utf-8");
    temper_store_turso::store::sync_adopt::adopt(db, &unreachable, TOKEN)
        .await
        .expect_err("no remote, no adoption");
    assert!(a_path.exists(), "a failed adoption puts the file back");
    assert!(!a_dir.path().join("app.db-info").exists());

    // A run that was killed after pushing some rows left its staging files.
    let staging = a_dir.path().join("app.db.adopting");
    let killed = server.open(&staging).await;
    write(&killed, "stray", 0).await;
    push(&killed).await;
    drop(killed);

    let report = temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect("adopt, starting over after the killed run");
    assert!(
        !staging.exists(),
        "the staged database took the file's place"
    );
    assert!(
        std::fs::read_dir(a_dir.path())
            .expect("dir")
            .flatten()
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("app.db.before-sync-")),
        "the original is kept aside"
    );
    assert!(report.contains("specs: 1 rows"), "{report}");
    temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect_err("already synced");

    let a = server.open(&a_path).await;
    assert_eq!(count(&a, "kept").await, 3);
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "kept").await, 3, "the rows reached the remote");
    let specs = b.load_specs().await.expect("specs");
    assert!(
        specs
            .iter()
            .any(|spec| spec.entity_type == "Note" && spec.ioa_source == "v1")
    );
}
