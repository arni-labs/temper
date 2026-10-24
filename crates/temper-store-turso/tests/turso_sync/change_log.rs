//! The local change log keeps only what the remote has not confirmed, and its
//! change numbers keep rising, so no change is skipped.

use std::sync::atomic::Ordering;

use temper_store_turso::TursoEventStore;

use super::support::*;

#[tokio::test]
async fn pushed_changes_leave_the_change_log() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let a = server.open(&path).await;
    let copy = a.sync_copy().expect("copy").clone();
    let mut newest = copy.last_change().await.expect("newest change");
    for n in 0..30 {
        write(&a, "c1", n).await;
        push(&a).await;
        let now = copy.last_change().await.expect("newest change");
        assert!(now > newest, "change numbers rise: {now} after {newest}");
        newest = now;
    }

    let rows = copy.change_log_rows().await.expect("change log rows");
    assert!(
        rows <= 5,
        "30 pushed writes leave {rows} change log rows; the remote has them all"
    );

    // What is not pushed yet stays.
    write(&a, "c1", 30).await;
    assert!(copy.change_log_rows().await.expect("rows") > rows);
    push(&a).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "c1").await, 31);
}

#[tokio::test]
async fn no_change_is_skipped_across_a_trim_a_restart_and_a_failed_push() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, on) = switch(&server.url).await;
    let open = |path: std::path::PathBuf| {
        let url = url.clone();
        async move {
            TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
                .await
                .expect("open")
        }
    };
    let (_a_dir, a_path) = machine();
    let a = open(a_path.clone()).await;
    for n in 0..5 {
        write(&a, "k1", n).await;
        push(&a).await;
    }
    let before_restart = a
        .sync_copy()
        .expect("copy")
        .last_change()
        .await
        .expect("newest change");
    drop(a);

    let a = open(a_path.clone()).await;
    let copy = a.sync_copy().expect("copy").clone();
    assert_eq!(
        copy.last_change().await.expect("newest change"),
        before_restart,
        "a restart finds the newest change number where the last run left it"
    );
    write(&a, "k1", 5).await;
    assert!(copy.last_change().await.expect("newest change") > before_restart);

    // A push that fails confirms and trims nothing.
    on.store(false, Ordering::Release);
    copy.push_committed()
        .await
        .expect_err("the remote cannot be reached");
    on.store(true, Ordering::Release);
    write(&a, "k1", 6).await;
    copy.push_committed_within(std::time::Duration::from_secs(30))
        .await
        .expect("push once the remote is back");

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "k1").await, 7, "every change reached the remote");
}
