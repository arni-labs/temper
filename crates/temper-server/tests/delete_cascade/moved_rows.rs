//! Rows whose reference moved while its projection write failed: the index
//! still puts them on their old owner, so a delete reads them from their
//! actors before it decides.

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;

use super::{Store, album_with_tracks, delete, field, get, open, server_on};
use crate::common::projection_writes_fail::ProjectionWritesFail;

/// A server whose projection writes fail while the returned switch is on.
async fn projection_failing_server(
    name: &str,
    store: &Store,
) -> (ServerState, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let (stack, failing) = ProjectionWritesFail::stack(open(store).await);
    (server_on(name, stack), failing)
}

/// Commit a change to `entity_type('id')` whose projection write fails, so
/// the query index keeps the fields it had before.
async fn change_unprojected(
    state: &ServerState,
    projection_fails: &std::sync::atomic::AtomicBool,
    entity_type: &str,
    id: &str,
    fields: serde_json::Value,
) {
    projection_fails.store(true, Ordering::SeqCst);
    let changed = state
        .update_tenant_entity_fields(&TenantId::default(), entity_type, id, fields.clone(), false)
        .await;
    projection_fails.store(false, Ordering::SeqCst);
    assert!(
        changed
            .as_ref()
            .is_err_and(|error| error.contains("query projection write failed")),
        "the change commits and reports its failed projection: {changed:?}"
    );
    let committed = state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .expect("read the changed row");
    for (field, value) in fields.as_object().expect("fields object") {
        assert_eq!(&committed.state.fields[field], value, "{id}.{field}");
    }
}

#[tokio::test]
async fn a_track_moved_to_another_album_is_not_deleted_with_its_old_one() {
    let store = Store::new("moved-away");
    let (state, projection_fails) =
        projection_failing_server("delete-cascade-moved-away", &store).await;
    album_with_tracks(&state, "a-old", &["t-1", "t-2", "t-moved"]).await;
    album_with_tracks(&state, "a-new", &[]).await;

    // The move commits, but the index still says t-moved is on a-old.
    change_unprojected(
        &state,
        &projection_fails,
        "Track",
        "t-moved",
        serde_json::json!({ "album_id": "a-new" }),
    )
    .await;

    let (status, body) = delete(&state, "Albums", "a-old").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "old album delete: {body}");
    assert_eq!(get(&state, "Albums", "a-old").await, StatusCode::NOT_FOUND);
    for track in ["t-1", "t-2"] {
        assert_eq!(
            get(&state, "Tracks", track).await,
            StatusCode::NOT_FOUND,
            "{track} outlived its album"
        );
    }
    assert_eq!(
        get(&state, "Tracks", "t-moved").await,
        StatusCode::OK,
        "a track on another album is not deleted with its old one"
    );
    assert_eq!(field(&state, "Track", "t-moved", "album_id").await, "a-new");
}

#[tokio::test]
async fn a_track_moved_into_an_album_is_deleted_with_it() {
    let store = Store::new("moved-in");
    let (state, projection_fails) =
        projection_failing_server("delete-cascade-moved-in", &store).await;
    album_with_tracks(&state, "a-target", &["t-1"]).await;
    album_with_tracks(&state, "a-other", &["t-other", "t-moved"]).await;

    // The move commits, but the index still says t-moved is on a-other.
    change_unprojected(
        &state,
        &projection_fails,
        "Track",
        "t-moved",
        serde_json::json!({ "album_id": "a-target" }),
    )
    .await;

    let (status, body) = delete(&state, "Albums", "a-target").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "target delete: {body}");
    for track in ["t-1", "t-moved"] {
        assert_eq!(
            get(&state, "Tracks", track).await,
            StatusCode::NOT_FOUND,
            "{track} outlived its album"
        );
    }
    assert_eq!(get(&state, "Albums", "a-other").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-other").await, StatusCode::OK);
}
