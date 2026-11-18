//! A record's meaning row (ADR-0186) is written, kept and removed with its query-plane row.

use temper_runtime::persistence::text_hash;

use super::*;

const TYPE: &str = "Panel";

async fn save(store: &PostgresEventStore, expected: u64, rows: &Rows) {
    store
        .append_with_rows(
            PID,
            expected,
            &[envelope("Moved", serde_json::json!({}))],
            rows.commit(),
        )
        .await
        .unwrap();
}

#[test]
fn upsert_writes_text_and_hash_without_a_vector() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        save(
            &store,
            0,
            &Rows::at(1, serde_json::json!({"title": "Ferry to the coast"})),
        )
        .await;
        let row = store
            .meaning_row(TENANT, TYPE, "p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.text, "Ferry to the coast");
        assert_eq!(row.model_tag, "");
        assert_eq!(row.vector, None);
        let pending = store.meaning_pending(TENANT, "m1", 10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].text_hash, text_hash("Ferry to the coast"));
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn vector_with_the_current_hash_is_kept_and_listed() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        save(
            &store,
            0,
            &Rows::at(1, serde_json::json!({"title": "Ferry"})),
        )
        .await;
        let kept = store
            .set_meaning_vector(TENANT, TYPE, "p1", &text_hash("Ferry"), "m1", &[0.5, 1.0])
            .await
            .unwrap();
        assert!(kept);
        let rows = store.meaning_rows(TENANT, TYPE, None, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_tag, "m1");
        assert_eq!(rows[0].vector, Some(vec![0.5, 1.0]));
        assert!(
            store
                .meaning_rows(TENANT, TYPE, Some("p1"), 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .meaning_pending(TENANT, "m1", 10)
                .await
                .unwrap()
                .is_empty()
        );
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn same_text_keeps_the_vector_and_changed_text_clears_it() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        save(
            &store,
            0,
            &Rows::at(1, serde_json::json!({"title": "Ferry", "n": 1})),
        )
        .await;
        store
            .set_meaning_vector(TENANT, TYPE, "p1", &text_hash("Ferry"), "m1", &[0.5, 1.0])
            .await
            .unwrap();
        // Same text, other fields: the vector stays.
        save(
            &store,
            1,
            &Rows::at(2, serde_json::json!({"title": "Ferry", "n": 2})),
        )
        .await;
        let row = store
            .meaning_row(TENANT, TYPE, "p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.vector, Some(vec![0.5, 1.0]));
        assert!(
            store
                .meaning_pending(TENANT, "m1", 10)
                .await
                .unwrap()
                .is_empty()
        );
        // New text: the vector is cleared and the row waits for the model.
        save(
            &store,
            2,
            &Rows::at(3, serde_json::json!({"title": "Bridge"})),
        )
        .await;
        let row = store
            .meaning_row(TENANT, TYPE, "p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.text.as_str(), row.model_tag.as_str(), row.vector),
            ("Bridge", "", None)
        );
        let pending = store.meaning_pending(TENANT, "m1", 10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].entity_id, "p1");
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn stale_hash_is_not_kept() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        save(
            &store,
            0,
            &Rows::at(1, serde_json::json!({"title": "Bridge"})),
        )
        .await;
        let kept = store
            .set_meaning_vector(TENANT, TYPE, "p1", &text_hash("Ferry"), "m1", &[1.0])
            .await
            .unwrap();
        assert!(!kept);
        let row = store
            .meaning_row(TENANT, TYPE, "p1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.vector, None);
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn projection_remove_deletes_the_row() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        save(
            &store,
            0,
            &Rows::at(1, serde_json::json!({"title": "Ferry"})),
        )
        .await;
        assert_eq!(count(&store, &pool, "entity_meaning").await, 1);
        store
            .append_with_rows(
                PID,
                1,
                &[tombstone()],
                CommitRows {
                    projection: Some(ProjectionRow::Remove),
                    state_timer: Some(StateTimerRow::End),
                    ..CommitRows::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(count(&store, &pool, "entity_meaning").await, 0);
        assert!(
            store
                .meaning_row(TENANT, TYPE, "p1")
                .await
                .unwrap()
                .is_none()
        );
        drop_schema(&pool, &schema).await;
    });
}
