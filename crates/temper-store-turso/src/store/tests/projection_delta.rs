//! The delta is co-committed, sequence guarded, durable, and tenant scoped.
use super::*;

async fn candidates(store: &TursoEventStore, tenant: &str) -> (Vec<String>, Vec<String>) {
    store
        .reconciled_equality_candidates(
            tenant,
            "Payment",
            &[("app".into(), "needle".into())],
            Some(100),
        )
        .await
        .unwrap()
}

async fn project(store: &TursoEventStore, id: &str, app: &str, sequence: u64) {
    store
        .upsert_query_projection(
            "t",
            "Payment",
            id,
            "Draft",
            &serde_json::json!({"Id": id, "app": app}),
            sequence,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn projection_delta_tracks_commits_and_only_retires_applied_sequences() {
    let store = make_store("projection-delta").await;
    for id in ["changed", "match", "missing", "noise"] {
        store
            .append(
                &format!("t:Payment:{id}"),
                0,
                &[test_envelope("Create", serde_json::json!({}))],
            )
            .await
            .unwrap();
    }
    project(&store, "changed", "old", 1).await;
    project(&store, "match", "needle", 1).await;
    project(&store, "noise", "other", 1).await;
    assert_eq!(
        candidates(&store, "t").await,
        (
            vec!["match".into(), "missing".into()],
            vec!["missing".into()]
        )
    );
    store
        .append(
            "t:Payment:changed",
            1,
            &[test_envelope("Update", serde_json::json!({}))],
        )
        .await
        .unwrap();
    // A projection arriving late must not erase a newer commit's mark.
    project(&store, "changed", "old", 1).await;
    assert_eq!(candidates(&store, "t").await.1, vec!["changed", "missing"]);
    project(&store, "changed", "needle", 2).await;
    assert_eq!(
        candidates(&store, "t").await.0,
        vec!["changed", "match", "missing"]
    );
    assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
    // Unchanged projections advance only counters, and still retire the mark.
    store
        .append(
            "t:Payment:changed",
            2,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
        .await
        .unwrap();
    project(&store, "changed", "needle", 3).await;
    assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
    assert!(candidates(&store, "other").await.0.is_empty());
    // An optimistic concurrency rejection must not leave a phantom mark.
    assert!(
        store
            .append(
                "t:Payment:noise",
                0,
                &[test_envelope("Touch", serde_json::json!({}))]
            )
            .await
            .is_err()
    );
    assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
    // Pending ids must be prioritized even when their id sorts after all
    // projected matches. Otherwise an empty bounded gap could falsely vouch
    // for a broader native page.
    let bounded = store
        .reconciled_equality_candidates("t", "Payment", &[("app".into(), "needle".into())], Some(1))
        .await
        .unwrap();
    assert_eq!(bounded.0.len(), 2);
    assert_eq!(bounded.1, vec!["missing"]);
    let complete = store
        .reconciled_equality_candidates("t", "Payment", &[("app".into(), "needle".into())], None)
        .await
        .unwrap();
    assert_eq!(complete.0, vec!["changed", "match", "missing"]);
    assert_eq!(complete.1, vec!["missing"]);
}

#[tokio::test]
async fn projection_delta_covers_batch_snapshot_wrapped_and_removed_rows() {
    let store = make_store("projection-delta-batch").await;
    store
        .append_batch(&[PersistenceAppend {
            persistence_id: "t:Payment:batch".into(),
            expected_sequence: 0,
            events: vec![test_envelope("Create", serde_json::json!({}))],
        }])
        .await
        .unwrap();
    store
        .save_snapshot("t:Payment:snapshot", 7, b"{}")
        .await
        .unwrap();
    store
        .upsert_query_projection(
            "t",
            "Payment",
            "wrapped",
            "Draft",
            &serde_json::json!({"fields": {"app": "needle"}}),
            1,
        )
        .await
        .unwrap();
    project(&store, "removed", "other", 1).await;
    store
        .remove_query_projection("t", "Payment", "removed")
        .await
        .unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["batch", "removed", "snapshot", "wrapped"]
    );
    project(&store, "snapshot", "other", 7).await;
    store
        .save_snapshot("t:Payment:snapshot", 7, b"{}")
        .await
        .unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["batch", "removed", "wrapped"]
    );
}

#[tokio::test]
async fn projection_delta_backfill_is_atomic_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file:{}", dir.path().join("old.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    for id in ["behind", "current", "missing", "unindexed", "wrapped"] {
        store
            .append(
                &format!("t:Payment:{id}"),
                0,
                &[test_envelope("Create", serde_json::json!({}))],
            )
            .await
            .unwrap();
        if id != "missing" {
            project(&store, id, "other", 1).await;
        }
    }
    store
        .append(
            "t:Payment:behind",
            1,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
        .await
        .unwrap();
    store
        .upsert_query_projection(
            "t",
            "Payment",
            "wrapped",
            "Draft",
            &serde_json::json!({"fields": {"app": "needle"}}),
            1,
        )
        .await
        .unwrap();
    // Recreate a pre-feature database, including a row from before the field index.
    {
        let conn = store
            .write_connection("test", super::super::WritePriority::High)
            .await
            .unwrap();
        conn.execute("DROP TABLE entity_projection_delta", ())
            .await
            .unwrap();
        conn.execute(
            "DELETE FROM entity_field_index WHERE entity_id = 'unindexed'",
            (),
        )
        .await
        .unwrap();
    }
    drop(store);
    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["behind", "missing", "unindexed", "wrapped"]
    );
    project(&store, "behind", "other", 2).await;
    project(&store, "unindexed", "other", 1).await;
    assert!(
        candidates(&store, "t")
            .await
            .1
            .contains(&"unindexed".to_string()),
        "an unchanged projection must not vouch for a missing legacy index"
    );
    drop(store);
    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["missing", "unindexed", "wrapped"]
    );
}

#[tokio::test]
async fn projection_delta_never_resurrects_a_tombstone_or_scans_deleted_rows() {
    let store = make_store("projection-delta-deleted").await;
    store
        .append(
            "t:Payment:gone",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
        )
        .await
        .unwrap();
    project(&store, "gone", "needle", 1).await;
    store
        .append("t:Payment:gone", 1, &[tombstone_envelope()])
        .await
        .unwrap();
    assert!(
        candidates(&store, "t").await.0.is_empty(),
        "the stale index must not return a deleted row"
    );
    store
        .remove_query_projection("t", "Payment", "gone")
        .await
        .unwrap();
    // A queued projection can arrive after removal; the terminal marker survives
    // it and is excluded through the pending index, not hydrated on each read.
    project(&store, "gone", "needle", 1).await;
    assert!(candidates(&store, "t").await.0.is_empty());
    assert!(candidates(&store, "t").await.1.is_empty());
}
