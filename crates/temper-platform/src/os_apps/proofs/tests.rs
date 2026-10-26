//! Incomplete results belong to an entire install, including staggered retries.

use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

async fn request(
    state: &PlatformState,
    attempt: &mut Attempt,
    digest: &str,
    walk: impl FnOnce(Bound<'_>) -> Walked + Send + 'static,
) -> Proof {
    proof(
        state,
        "retry-test",
        "retry-test",
        digest.to_string(),
        Keys {
            model: digest.to_string(),
            source: digest.to_string(),
        },
        attempt,
        walk,
    )
    .await
}

fn incomplete() -> Walked {
    Walked {
        verdict: Verdict {
            held: vec![],
            refused: Some("no room".to_string()),
        },
        fit: false,
    }
}

#[tokio::test]
async fn staggered_incomplete_rules_are_retained_for_every_install_retry() {
    let state = PlatformState::new(None);
    let mut attempt = Attempt::default();
    let a = format!("retry-a-{}", uuid::Uuid::new_v4());
    let b = format!("retry-b-{}", uuid::Uuid::new_v4());
    let starts = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&starts);
    let Proof::Running(first) = request(&state, &mut attempt, &a, move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        incomplete()
    })
    .await
    else {
        panic!("first rule starts")
    };
    first.ended().await;

    let (finish_b, wait_b) = std::sync::mpsc::channel();
    let count = Arc::clone(&starts);
    let Proof::Running(second) = request(&state, &mut attempt, &b, move |_| {
        count.fetch_add(1, Ordering::SeqCst);
        wait_b
            .recv_timeout(Duration::from_secs(10))
            .expect("release second rule");
        incomplete()
    })
    .await
    else {
        panic!("second rule starts")
    };
    // First retry consumes A's shared result while B still runs.
    assert!(matches!(
        request(&state, &mut attempt, &a, |_| panic!("A restarted")).await,
        Proof::Done(_)
    ));
    assert!(matches!(
        request(&state, &mut attempt, &b, |_| panic!("B restarted")).await,
        Proof::Running(_)
    ));
    finish_b.send(()).expect("finish B");
    second.ended().await;
    // Every later retry still sees A, even after the shared entry was consumed.
    for _ in 0..3 {
        for digest in [&a, &b] {
            let Proof::Done(verdict) = request(&state, &mut attempt, digest, |_| {
                panic!("completed rule restarted")
            })
            .await
            else {
                panic!("install must finish once both rules ended")
            };
            assert_eq!(verdict.refused.as_deref(), Some("no room"));
        }
    }
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    // An independent install can retry work that did not fit.
    let Proof::Running(retried) =
        request(&state, &mut Attempt::default(), &a, |_| incomplete()).await
    else {
        panic!("later install retries incomplete work")
    };
    retried.ended().await;
}
