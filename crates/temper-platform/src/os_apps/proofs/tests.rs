//! A walk's verdict reaches every install waiting on it, and stays with
//! each through its retries; one that did not fit is walked again later.

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
            refused: None,
            unproved: Some("no room".to_string()),
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
    first.ended(&mut attempt).await.unwrap();

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
    // A's verdict stays with the install while B still runs.
    assert!(matches!(
        request(&state, &mut attempt, &a, |_| panic!("A restarted")).await,
        Proof::Done(_)
    ));
    assert!(matches!(
        request(&state, &mut attempt, &b, |_| panic!("B restarted")).await,
        Proof::Running(_)
    ));
    finish_b.send(()).expect("finish B");
    second.ended(&mut attempt).await.unwrap();
    // Every later retry still sees both.
    for _ in 0..3 {
        for digest in [&a, &b] {
            let Proof::Done(verdict) = request(&state, &mut attempt, digest, |_| {
                panic!("completed rule restarted")
            })
            .await
            else {
                panic!("install must finish once both rules ended")
            };
            assert_eq!(verdict.unproved.as_deref(), Some("no room"));
        }
    }
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    // An independent install can retry work that did not fit.
    let Proof::Running(retried) =
        request(&state, &mut Attempt::default(), &a, |_| incomplete()).await
    else {
        panic!("later install retries incomplete work")
    };
    retried.ended(&mut Attempt::default()).await.unwrap();
}

#[tokio::test]
async fn every_install_waiting_on_a_walk_gets_its_verdict() {
    let state = PlatformState::new(None);
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let key = format!("shared-{}", uuid::Uuid::new_v4());
    let (mut one, mut two) = (Attempt::default(), Attempt::default());
    let Proof::Running(first) = request(&state, &mut one, &key, move |_| {
        blocked.recv().expect("release the walk");
        incomplete()
    })
    .await
    else {
        panic!("the first install starts the walk")
    };
    let Proof::Running(second) = request(&state, &mut two, &key, |_| {
        panic!("an install waiting on a walk must not start another")
    })
    .await
    else {
        panic!("the second install waits on the same walk")
    };
    release.send(()).expect("release the walk");
    first.ended(&mut one).await.unwrap();
    // The first install has read its verdict before the second reads its own.
    second.ended(&mut two).await.unwrap();
    for attempt in [&mut one, &mut two] {
        let Proof::Done(verdict) =
            request(&state, attempt, &key, |_| panic!("the walk ran again")).await
        else {
            panic!("each install keeps the verdict it waited for")
        };
        assert_eq!(verdict.unproved.as_deref(), Some("no room"));
    }
}
