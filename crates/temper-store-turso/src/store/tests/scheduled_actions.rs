//! Scheduled-action rows commit with the event that arms them and with the
//! event that fires them. Cancellation durably clears the action before cleanup.

use temper_runtime::persistence::{
    CommitRows, EventStore, PersistenceError, ScheduledActionRecord,
};

use super::{make_store, test_envelope};

fn row(tenant: &str, entity_id: &str, arm_id: &str) -> ScheduledActionRecord {
    ScheduledActionRecord {
        tenant: tenant.to_string(),
        entity_type: "Job".to_string(),
        entity_id: entity_id.to_string(),
        arm_id: arm_id.to_string(),
        action: "Fire".to_string(),
        due_at: "2026-01-01T00:10:00+00:00".to_string(),
        armed_by: "{}".to_string(),
    }
}

fn arm_ids(rows: &[ScheduledActionRecord]) -> Vec<(String, String)> {
    let mut ids: Vec<_> = rows
        .iter()
        .map(|row| (row.entity_id.clone(), row.arm_id.clone()))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn scheduled_action_rows_commit_with_their_events() {
    let store = make_store("scheduled-actions").await;
    let armed = [
        row("tenant-a", "job-1", "1.0"),
        row("tenant-a", "job-1", "1.1"),
    ];

    // Arming: the rows land with the event, and only in their tenant.
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[test_envelope("Start", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &armed,
                ..CommitRows::default()
            },
        )
        .await
        .expect("arming append");
    store
        .append_with_rows(
            "tenant-b:Job:job-1",
            0,
            &[test_envelope("Start", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &[row("tenant-b", "job-1", "1.0")],
                ..CommitRows::default()
            },
        )
        .await
        .expect("other tenant's append");
    let rows = store.list_scheduled_actions("tenant-a").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().find(|r| r.arm_id == "1.0"), Some(&armed[0]));
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-b").await.unwrap()),
        vec![("job-1".to_string(), "1.0".to_string())]
    );

    // A refused append writes no row and removes none.
    let err = store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[test_envelope("Fire", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &[row("tenant-a", "job-1", "9.0")],
                fired_scheduled_action: Some("1.0"),
                ..CommitRows::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PersistenceError::ConcurrencyViolation { .. }));
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-a").await.unwrap()),
        vec![
            ("job-1".to_string(), "1.0".to_string()),
            ("job-1".to_string(), "1.1".to_string())
        ]
    );

    // Firing: the event removes its own row in the same commit, and no
    // other record's or tenant's row with the same arm id.
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            1,
            &[test_envelope("Fire", serde_json::json!({}))],
            CommitRows {
                fired_scheduled_action: Some("1.0"),
                ..CommitRows::default()
            },
        )
        .await
        .expect("firing append");
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-a").await.unwrap()),
        vec![("job-1".to_string(), "1.1".to_string())]
    );
    assert_eq!(
        store
            .list_scheduled_actions("tenant-b")
            .await
            .unwrap()
            .len(),
        1
    );

    // A refused call is durably cancelled before deletion. Repeating the
    // cancellation, including after deletion, is harmless and tenant-scoped.
    for _ in 0..2 {
        store
            .cancel_scheduled_action("tenant-a", "Job", "job-1", "1.1")
            .await
            .expect("cancellation");
    }
    let cancelled = store.list_scheduled_actions("tenant-a").await.unwrap();
    assert_eq!(cancelled.len(), 1);
    assert!(
        cancelled[0].action.is_empty(),
        "a restart sees terminal disposition"
    );
    assert_eq!(
        store.list_scheduled_actions("tenant-b").await.unwrap()[0].action,
        "Fire"
    );

    store
        .cancel_scheduled_action("tenant-a", "Job", "missing", "1.1")
        .await
        .expect("absent cancellation");
    // A removal outside a commit takes one row, and repeating it is a no-op.
    for _ in 0..2 {
        store
            .delete_scheduled_action("tenant-a", "Job", "job-1", "1.1")
            .await
            .expect("removal");
    }
    store
        .cancel_scheduled_action("tenant-a", "Job", "job-1", "1.1")
        .await
        .expect("cancellation after deletion");
    assert!(
        store
            .list_scheduled_actions("tenant-a")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .list_scheduled_actions("tenant-b")
            .await
            .unwrap()
            .len(),
        1
    );
}

/// The arm ids `table` holds for `tenant`, as a raw read of it sees them.
async fn arm_ids_in(store: &crate::TursoEventStore, table: &str, tenant: &str) -> Vec<String> {
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .unwrap();
    let mut rows = conn
        .query(
            &format!("SELECT arm_id FROM {table} WHERE tenant = ?1 ORDER BY arm_id"),
            crate::driver::params![tenant],
        )
        .await
        .unwrap();
    let mut ids = Vec::new();
    while let Some(row) = rows.next().await.unwrap() {
        ids.push(row.get::<String>(0).unwrap());
    }
    ids
}

/// Review rv-6457ca865: a release from before follow-on rows reads only
/// `scheduled_actions`, and would arm a follow-on row there as a scheduled
/// action, firing the move's own action again, as the kernel, after a
/// rollback. A follow-on row is kept in `follow_on_rows`, which such a
/// release never reads; this release lists and removes it with the
/// scheduled actions, and a follow-on row an earlier release left in
/// `scheduled_actions` is still listed and removed.
#[tokio::test]
async fn a_follow_on_row_is_kept_where_an_older_release_never_reads_it() {
    let store = make_store("follow-on-rows").await;
    let follow = "follow:caller-key";
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[test_envelope("Start", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &[
                    row("tenant-a", "job-1", "1.0"),
                    row("tenant-a", "job-1", follow),
                ],
                ..CommitRows::default()
            },
        )
        .await
        .expect("append");

    assert_eq!(
        arm_ids_in(&store, "scheduled_actions", "tenant-a").await,
        ["1.0"],
        "what a release from before follow-on rows reads holds no follow-on row"
    );
    assert_eq!(
        arm_ids_in(&store, "follow_on_rows", "tenant-a").await,
        [follow]
    );
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-a").await.unwrap()),
        vec![
            ("job-1".to_string(), "1.0".to_string()),
            ("job-1".to_string(), follow.to_string()),
        ]
    );
    assert_eq!(
        store
            .list_entity_scheduled_actions("tenant-a", "Job", "job-1")
            .await
            .unwrap()
            .len(),
        2
    );

    // A follow-on row an earlier release of this kernel left in
    // scheduled_actions is still listed, and its removal takes it.
    let legacy = "follow:left-by-6457ca865";
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .unwrap();
    conn.execute(
        "INSERT INTO scheduled_actions (tenant, entity_type, entity_id, arm_id, action, due_at, armed_by) \
         VALUES ('tenant-a', 'Job', 'job-1', ?1, 'Start', '2026-01-01T00:00:00+00:00', '{}')",
        crate::driver::params![legacy],
    )
    .await
    .unwrap();
    drop(conn);
    assert_eq!(
        store
            .list_scheduled_actions("tenant-a")
            .await
            .unwrap()
            .len(),
        3
    );

    for arm_id in [follow, legacy] {
        store
            .delete_scheduled_action("tenant-a", "Job", "job-1", arm_id)
            .await
            .expect("removal");
    }
    assert_eq!(
        arm_ids_in(&store, "follow_on_rows", "tenant-a").await,
        Vec::<String>::new()
    );
    assert_eq!(
        arm_ids_in(&store, "scheduled_actions", "tenant-a").await,
        ["1.0"]
    );
}
