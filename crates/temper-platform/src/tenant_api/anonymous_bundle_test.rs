//! The anonymous bundle export's places, as callers see them.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use tokio::sync::Semaphore;
use tower::ServiceExt;

use super::bundle_admission::{exports_that_fit, in_turn, places};
use super::*;
use crate::genesis_install::BUNDLE_EXPORT_HELD_BYTES;

/// A TemperPaw boot asks for every installed app at once, far more than there
/// are places. Every one of those exports must run, each answering with its
/// own bundle, and no more than the places at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exports_past_the_places_all_answer_with_their_own_bundle() {
    const PLACES: usize = 3;
    const EXPORTS: usize = 28;
    let places = Arc::new(Semaphore::new(PLACES));
    let running = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    let exports: Vec<_> = (0..EXPORTS)
        .map(|index| {
            let (places, running, most) = (places.clone(), running.clone(), most.clone());
            tokio::spawn(async move {
                in_turn(&places, async {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    running.fetch_sub(1, Ordering::SeqCst);
                    format!("bundle {index}").into_bytes()
                })
                .await
            })
        })
        .collect();
    for (index, export) in exports.into_iter().enumerate() {
        let bytes = export.await.expect("export task");
        assert_eq!(bytes, format!("bundle {index}").into_bytes());
    }
    assert_eq!(
        most.load(Ordering::SeqCst),
        PLACES,
        "every place is used, and no more"
    );
}

/// With every place taken, a request waits for one and then gets the real
/// answer about the app it named (here: there is no such public app), not a
/// busy refusal.
#[tokio::test]
async fn an_export_past_the_places_waits_for_one_instead_of_being_refused() {
    let taken = u32::try_from(places().available_permits()).expect("places fit u32");
    let held = places().acquire_many(taken).await.expect("places");
    let app = super::super::tenant_api_router().with_state(PlatformState::new(None));
    let request = Request::get(format!(
        "/genesis/apps/acme/notes/versions/{}/bundle",
        "1".repeat(40)
    ))
    .header("x-tenant-id", "default")
    .body(Body::empty())
    .expect("request");
    let mut answer = tokio::spawn(app.oneshot(request));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut answer)
            .await
            .is_err(),
        "no answer while every place is taken"
    );
    drop(held);
    let response = tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("an answer once a place is free")
        .expect("request task")
        .expect("answer");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .is_none()
    );
}

#[test]
fn the_places_are_the_exports_free_memory_holds_and_at_least_one() {
    assert_eq!(exports_that_fit(None), 1, "no figure: one at a time");
    assert_eq!(exports_that_fit(Some(0)), 1);
    assert_eq!(exports_that_fit(Some(BUNDLE_EXPORT_HELD_BYTES - 1)), 1);
    assert_eq!(exports_that_fit(Some(10 * BUNDLE_EXPORT_HELD_BYTES)), 10);
    assert_eq!(
        exports_that_fit(Some(11 * BUNDLE_EXPORT_HELD_BYTES - 1)),
        10,
        "a part of an export's memory is no place"
    );
}
