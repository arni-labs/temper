//! The anonymous bundle export's place limit, as callers see it.

use axum::body::Body;
use axum::http::Request;
use tower::ServiceExt;

use super::*;

#[test]
fn the_anonymous_bundle_cap_refuses_rather_than_queues_when_full() {
    let admission = anonymous_bundle_admission();
    let held: Vec<_> = (0..MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS)
        .map(|_| admission.try_acquire().expect("capacity available"))
        .collect();
    assert!(
        admission.try_acquire().is_err(),
        "the request past the cap must be refused, not queued behind a materialization"
    );
    drop(held);
    assert!(
        admission.try_acquire().is_ok(),
        "capacity returns when an export finishes"
    );
}

/// A boot fetching its installed apps at once meets the cap. The answer must
/// say Genesis is busy and when to come back, not that the caller is refused,
/// so the caller retries instead of giving the app up.
#[tokio::test]
async fn a_busy_anonymous_export_says_when_to_come_back_and_denies_nothing() {
    let admission = anonymous_bundle_admission();
    let _held: Vec<_> = (0..MAX_CONCURRENT_ANONYMOUS_BUNDLE_EXPORTS)
        .map(|_| admission.try_acquire().expect("capacity available"))
        .collect();
    let app = super::super::tenant_api_router().with_state(PlatformState::new(None));
    let response = app
        .oneshot(
            Request::get(format!(
                "/genesis/apps/acme/notes/versions/{}/bundle",
                "1".repeat(40)
            ))
            .header("x-tenant-id", "default")
            .body(Body::empty())
            .expect("request"),
        )
        .await
        .expect("answer");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let retry_after = response
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    assert_eq!(
        retry_after.as_deref(),
        Some("1"),
        "the answer says when to come back"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    let body = String::from_utf8_lossy(&body);
    assert!(
        !body.contains("denied"),
        "a busy export denies nothing: {body}"
    );
}
