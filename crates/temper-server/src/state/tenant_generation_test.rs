//! Tenant generations: a re-created id is a new generation, and work of the old one is refused.

use std::time::Duration;

use super::*;

#[test]
fn a_tenant_created_again_starts_a_new_generation_and_the_old_stamp_is_refused() {
    let generations = TenantGenerations::default();
    let acme = TenantId::new("acme");
    let first = generations.live(&acme).expect("a new id starts live");
    assert!(generations.lease(&first).is_some());

    generations.close(&acme);
    assert!(generations.live(&acme).is_none(), "closed while removed");
    assert!(generations.lease(&first).is_none());
    assert!(generations.lease_live(&acme).is_none());

    generations.forget(&acme);
    let second = generations
        .live(&acme)
        .expect("the id's next use starts again");
    assert!(second.generation() > first.generation());
    assert!(
        generations.lease(&first).is_none(),
        "work of the deleted generation is refused in the new one"
    );
    assert!(generations.lease(&second).is_some());
}

#[test]
fn forgetting_a_live_generation_keeps_it() {
    let generations = TenantGenerations::default();
    let acme = TenantId::new("acme");
    let live = generations.live(&acme).expect("live");
    generations.forget(&acme);
    assert_eq!(generations.live(&acme), Some(live));
}

#[tokio::test(start_paused = true)]
async fn a_closed_generation_drains_when_its_last_lease_ends() {
    let generations = TenantGenerations::default();
    let acme = TenantId::new("acme");
    let first = generations.lease_live(&acme).expect("live");
    let second = generations.lease_live(&acme).expect("live");
    let closed = generations.close(&acme);
    assert_eq!(
        closed.leases(),
        2,
        "closing keeps the work already admitted"
    );
    drop(first);
    let release = async {
        tokio::task::yield_now().await;
        drop(second);
    };
    let drained = tokio::time::timeout(Duration::from_secs(1), TenantGenerations::drained(&closed));
    let (drained, ()) = tokio::join!(drained, release);
    assert!(drained.is_ok());
    assert_eq!(closed.leases(), 0);
}

#[test]
fn queued_work_gets_no_lease_once_its_generation_is_deleted() {
    let generations = Arc::new(TenantGenerations::default());
    let acme = TenantId::new("acme");
    let queued = GenerationRef {
        generations: Arc::clone(&generations),
        stamp: generations.stamp(&acme),
    };
    assert!(
        queued.lease().is_some(),
        "written while its generation is live"
    );

    generations.close(&acme);
    assert!(
        queued.lease().is_none(),
        "dropped while the tenant is removed"
    );
    generations.forget(&acme);
    assert!(generations.live(&acme).is_some());
    assert!(
        queued.lease().is_none(),
        "dropped in the tenant created again under its id"
    );
}
