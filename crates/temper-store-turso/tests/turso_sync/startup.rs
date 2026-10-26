//! Starting a synced database: without the remote, from an older copy that
//! wrote on its own, from a damaged file, from a restore cut short, by
//! fetching, and from a database that was never synced.

use std::path::PathBuf;

use super::support::*;
use temper_store_turso::{CopyState, SyncCopy, TursoEventStore};

#[tokio::test]
async fn a_start_without_the_remote_serves_and_backs_up_once_it_answers() {
    // The drive is the database's home and the remote its backup, so a start
    // without the remote serves as usual. Whether the file is behind the
    // remote can be told only by asking the remote, so until then nothing is
    // pushed; what the file commits meanwhile is pushed after the check.
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
    write(&a, "u1", 1).await;
    assert_eq!(count(&a, "u1").await, 2, "and writes");
    copy.push_committed()
        .await
        .expect_err("nothing is pushed unchecked");
    copy.recheck().await.expect_err("still unreachable");

    on.store(true, std::sync::atomic::Ordering::Release);
    assert_eq!(copy.recheck().await.expect("recheck"), CopyState::Copying);
    write(&a, "u1", 2).await;
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(
        count(&b, "u1").await,
        3,
        "the write made unchecked was backed up too"
    );
}

#[tokio::test]
async fn an_older_copy_that_wrote_without_the_remote_keeps_serving_and_stops_the_backup() {
    // Each side then has changes the other lacks. Bringing the file level
    // with the remote would drop the file's own; pushing them would have the
    // remote skip the ones whose numbers it already holds. So the file keeps
    // serving, the backup stops, and an operator decides.
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
    // Writes go on while unchecked, here more changes than the remote is
    // ahead by, so the file's newest change number passes the remote's: only
    // the number at open tells.
    for i in 0..10 {
        write(&a, "meanwhile", i).await;
    }
    // A restart during the outage keeps the number from the first start.
    drop((a, copy));
    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(copy.state(), CopyState::Unchecked);
    on.store(true, std::sync::atomic::Ordering::Release);
    let state = copy.recheck().await.expect("recheck");
    assert!(
        matches!(&state, CopyState::Stopped(cause) if cause.contains("diverged")),
        "{state:?}"
    );
    assert_eq!(count(&a, "meanwhile").await, 10, "the file keeps its own");
    write(&a, "meanwhile", 10).await;
    assert_eq!(count(&a, "meanwhile").await, 11, "and confirms new writes");
    let error = copy
        .push_committed()
        .await
        .expect_err("the backup is stopped");
    assert!(error.to_string().contains("diverged"), "{error}");
    drop((a, copy));

    let a = open(a_path.clone()).await;
    assert!(
        matches!(a.sync_copy().expect("copy").state(), CopyState::Stopped(_)),
        "the stop is kept across a restart"
    );
    assert_eq!(count(&a, "meanwhile").await, 11);
    assert_eq!(aside_dirs(a_dir.path()), 0, "nothing was replaced");
    let status = SyncCopy::describe(a_path.to_str().expect("utf-8")).expect("status");
    assert!(status.contains("stopped: diverged"), "{status}");
}

#[tokio::test]
async fn a_damaged_file_is_set_aside_and_restored_from_the_remote() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    for i in 0..20 {
        write(&a, "kept", i).await;
    }
    push(&a).await;
    // Every page in the database file, none in the log, so the damage below
    // is what the next start reads.
    while !a
        .sync_copy()
        .expect("copy")
        .checkpoint_if_wal_over(0)
        .await
        .expect("checkpoint")
    {
        tokio::task::yield_now().await;
    }
    drop(a);

    let root = {
        let db = turso::Builder::new_local(a_path.to_str().expect("utf-8"))
            .build()
            .await
            .expect("read the file");
        let conn = db.connect().expect("connect");
        let mut rows = conn
            .query(
                "SELECT rootpage FROM sqlite_master WHERE name = 'events'",
                (),
            )
            .await
            .expect("query");
        let row = rows.next().await.expect("row").expect("events table");
        row.get::<i64>(0).expect("rootpage")
    };
    let bytes = std::fs::read(&a_path).expect("read db");
    let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65536,
        size => usize::from(size),
    };
    let offset = (usize::try_from(root).expect("page") - 1) * page_size;
    let mut damaged = bytes;
    damaged[offset..offset + page_size].fill(0xFF);
    std::fs::write(&a_path, damaged).expect("damage the file");

    let a = server.open(&a_path).await;
    assert_eq!(count(&a, "kept").await, 20, "restored from the remote");
    assert_eq!(a.sync_copy().expect("copy").state(), CopyState::Copying);
    let aside = std::fs::read_dir(a_dir.path())
        .expect("read dir")
        .map(|entry| entry.expect("entry").path())
        .find(|entry| entry.is_dir())
        .expect("the damaged file is kept aside");
    assert!(aside.join("app.db").exists());
}

#[tokio::test]
async fn a_restore_cut_short_is_kept_aside_and_run_again_at_the_next_start() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "whole", 0).await;
    push(&a).await;
    drop(a);

    // What a start stopped part way through a restore leaves: some of the
    // remote's pages, the restore's marker, and no sync metadata.
    let (b_dir, b_path) = machine();
    std::fs::write(&b_path, vec![7u8; 64 * 1024]).expect("partial file");
    std::fs::write(b_dir.path().join("app.db-sync-restoring"), b"").expect("marker");

    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "whole").await, 1, "the restore ran again in full");
    assert!(
        !b_dir.path().join("app.db-sync-restoring").exists(),
        "a finished restore clears its marker"
    );
    let kept = std::fs::read_dir(b_dir.path())
        .expect("read dir")
        .map(|entry| entry.expect("entry").path())
        .find(|entry| entry.is_dir())
        .expect("the partial file is kept aside");
    assert_eq!(
        std::fs::metadata(kept.join("app.db")).expect("kept").len(),
        64 * 1024
    );
}

#[tokio::test]
async fn a_fetch_downloads_the_remote_beside_and_never_over_a_database() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_a_dir, a_path) = machine();
    let a = server.open(&a_path).await;
    write(&a, "fetched", 0).await;
    push(&a).await;
    drop(a);

    let (b_dir, b_path) = machine();
    let b = b_path.to_str().expect("utf-8");
    let report = SyncCopy::fetch(b, &server.url, TOKEN).await.expect("fetch");
    assert!(report.contains("synced: client"), "{report}");
    assert!(b_dir.path().join("app.db-info").exists(), "{report}");
    assert!(
        !b_dir.path().join("app.db-sync-restoring").exists(),
        "a finished fetch is not a partial restore"
    );

    let error = SyncCopy::fetch(b, &server.url, TOKEN)
        .await
        .expect_err("a second fetch would land on the first");
    assert!(error.to_string().contains("already exists"), "{error}");

    // Moved into place by hand, the fetched file opens as it is.
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "fetched").await, 1);
    assert_eq!(b.sync_copy().expect("copy").state(), CopyState::Copying);
    let aside = std::fs::read_dir(b_dir.path())
        .expect("read dir")
        .filter(|entry| entry.as_ref().expect("entry").path().is_dir())
        .count();
    assert_eq!(aside, 0, "nothing was restored again");
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

    // A run that began copying, then was killed or failed, pushed some rows.
    let staging = a_dir.path().join("app.db.adopting");
    let killed = server.open(&staging).await;
    write(&killed, "stray", 0).await;
    push(&killed).await;
    drop(killed);
    temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect_err("a remote holding events no run of this adopt began on is refused");
    let started = a_dir.path().join("app.db.adopt-started");
    std::fs::write(&started, b"").expect("mark");

    let report = temper_store_turso::store::sync_adopt::adopt(db, &server.url, TOKEN)
        .await
        .expect("adopt, starting over after the run that began copying");
    assert!(!started.exists(), "the mark goes once the file is moved");
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
