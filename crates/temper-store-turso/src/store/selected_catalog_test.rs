use super::*;

async fn store() -> (TursoEventStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file:{}", dir.path().join("selected.db").display());
    (TursoEventStore::new(&url, None).await.unwrap(), dir)
}

#[tokio::test]
async fn a_selected_read_returns_only_the_selected_values_from_where_a_whole_read_finds_them() {
    let (store, _dir) = store().await;
    let fields = serde_json::json!({
        "Id": "job-1",
        "Name": "column name",
        "Large": "x".repeat(4096),
        "last_event_by": "forged",
    });
    let state = serde_json::json!({
        "status": "Open",
        "Title": "state title",
        "fields": {"Name": "state name wins"},
        "first_event_at": "2026-01-01T00:00:00Z",
        "last_event_at": "2026-01-01T00:00:00.5Z",
        "total_event_count": 3,
    });
    store
        .upsert_query_projection_with_state("t", "Job", "job-1", "Open", &fields, &state, 3)
        .await
        .unwrap();
    let ids = ["job-1".to_string(), "missing".to_string()];
    let select = |names: &[&str]| {
        names
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()
    };

    let rows = store
        .load_selected_entity_catalog_rows(
            "t",
            "Job",
            &ids,
            &select(&[
                "Id",
                "Name",
                "Title",
                "total_event_count",
                "Missing",
                "status",
            ]),
        )
        .await
        .unwrap()
        .expect("a selected read is supported");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].entity_id, "job-1");
    assert_eq!(rows[0].status, "Open");
    assert_eq!(rows[0].sequence_nr, 3);
    assert_eq!(rows[0].state, None);
    assert_eq!(
        rows[0].fields,
        serde_json::json!({
            "Id": "job-1",
            "Name": "state name wins",
            "Title": "state title",
            "total_event_count": 3,
        })
    );

    // Kernel metadata comes from the state alone, `first_event_at` from its
    // column: an app field of the same name never supplies it.
    let rows = store
        .load_selected_entity_catalog_rows(
            "t",
            "Job",
            &ids,
            &select(&["first_event_at", "last_event_at", "last_event_by"]),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rows[0].state,
        Some(serde_json::json!({
            "first_event_at": "2026-01-01T00:00:00Z",
            "last_event_at": "2026-01-01T00:00:00.5Z",
        }))
    );
    assert_eq!(rows[0].fields, serde_json::json!({}));

    // Whole documents load as whole rows.
    assert!(
        store
            .load_selected_entity_catalog_rows("t", "Job", &ids, &select(&["fields"]))
            .await
            .unwrap()
            .is_none()
    );
}
