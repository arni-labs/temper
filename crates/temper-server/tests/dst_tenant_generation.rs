//! DST: a deleted tenant's work never reaches the tenant created again under its id.
//!
//! Each case deletes a tenant while some of its work is still under way, creates the tenant again under the same id
//! and checks that nothing of the deleted one acts afterwards:
//! - an actor admitted while the removal runs is not running when the storage goes;
//! - a projection write queued before the delete does not land in the tenant created again;
//! - a state timer armed before the delete does not fire into the record created again;
//! - a scheduled follow-on of the deleted record does not fire into the record created again with the same id,
//!   although both records reach the same event count, so (tenant, type, id, event count, effect index) repeats;
//! - a second delete that was under way when the first finished and the tenant was created again leaves the new
//!   tenant's specs and records in place.
//!
//! One task drives every schedule (`tokio::join!`, no spawned test tasks) on a paused clock; the seed picks where the
//! competing step lands and how long the store is slow.

#[path = "dst_tenant_generation/harness.rs"]
mod harness;

use std::time::Duration;

use harness::{Harness, acme, live_actors};
use temper_runtime::scheduler::install_deterministic_context;

const SEEDS: u64 = 48;

/// The seed's pick from `0..n`.
fn pick(seed: u64, salt: u64, n: u64) -> u64 {
    seed.wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(salt.wrapping_mul(1_442_695_040_888_963_407))
        .rotate_right(29)
        % n
}

async fn yields(count: u64) {
    for _ in 0..count {
        tokio::task::yield_now().await;
    }
}

/// Invariant: the removal finishes, and when the tenant's storage is removed no actor of the tenant is running,
/// including one admitted after the removal began.
#[tokio::test(start_paused = true)]
async fn dst_tenant_generation_delete_racing_actor_creation() {
    let mut violations = Vec::new();
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let harness = Harness::new(seed);
        // A live actor, so the removal waits for one to end and the creator can land inside it.
        assert!(harness.dispatch("b1", "Ring").await.is_ok(), "seed {seed}");

        let creator_after = pick(seed, 1, 12);
        let storage_after = pick(seed, 2, 6);
        let live_at_removal = std::cell::Cell::new(None);
        let tenant = acme();
        let stamp = harness.state.tenant_generations.stamp(&tenant);
        let removal = harness.state.remove_tenant(&stamp, || async {
            yields(storage_after).await;
            live_at_removal.set(Some(live_actors(&harness.state)));
            Ok(harness.remove_storage())
        });
        let creator = async {
            yields(creator_after).await;
            // The tenant is created again while its removal may still run.
            harness.register();
            let _ = harness.dispatch("b2", "Ring").await;
        };
        let (removed, ()) = tokio::join!(removal, creator);
        let schedule =
            format!("creator after {creator_after} yields, storage after {storage_after}");
        match (removed, live_at_removal.get()) {
            (Ok(_), Some(0)) => {}
            (Ok(_), live) => violations.push(format!(
                "seed {seed} ({schedule}): {live:?} actor(s) of the tenant were running when its storage was removed"
            )),
            // An actor admitted during the removal was neither refused nor halted with the others: the removal
            // waited for it and gave up.
            (Err(error), _) => violations.push(format!(
                "seed {seed} ({schedule}): the removal did not finish: {error}"
            )),
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

/// Invariant: after the tenant is created again, its projection row for a record is the new record's, never a write
/// of the deleted tenant that was still queued when the delete ran.
#[tokio::test(start_paused = true)]
async fn dst_tenant_generation_projection_write_after_delete() {
    let mut violations = Vec::new();
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let harness = Harness::new(seed);
        assert!(harness.dispatch("b1", "Touch").await.is_ok(), "seed {seed}");
        // The commit's inline projection fails, so the write goes to the retrying queue, which keeps failing for a
        // while: the store is slow to recover. The old record is one event ahead of the new one, so its write would
        // pass the store's sequence guard.
        let failures = 2 + pick(seed, 3, 3) as usize;
        harness.rows.fail_next(failures);
        let old = harness.dispatch("b1", "Ring").await.expect("ring");
        assert_eq!(old.state.status, "Ringing");
        yields(pick(seed, 4, 4)).await;

        harness.delete().await;
        harness.register();
        let new = harness.dispatch("b1", "Touch").await.expect("touch");
        assert_eq!(
            new.state.status, "Idle",
            "seed {seed}: the record starts again"
        );
        tokio::time::sleep(Duration::from_secs(5)).await;

        let row = harness.rows.row("b1");
        if row != Some(("Idle".to_string(), new.state.sequence_nr)) {
            violations.push(format!(
                "seed {seed} ({failures} failed writes): projection row of the re-created record is {row:?}, \
                 expected the new record's (Idle, {})",
                new.state.sequence_nr
            ));
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

/// Invariant: a state timer armed in the deleted tenant does not fire into the record created again under its id.
#[tokio::test(start_paused = true)]
async fn dst_tenant_generation_timer_after_delete_and_recreate() {
    let mut violations = Vec::new();
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let harness = Harness::new(seed);
        // The old timer fires at 10 s; the new record's own timer, armed at `deleted_at`, fires at deleted_at + 10.
        assert!(harness.dispatch("b1", "Ring").await.is_ok(), "seed {seed}");
        let deleted_at = Duration::from_millis(1_000 + pick(seed, 5, 8_000));
        tokio::time::sleep(deleted_at).await;
        harness.delete().await;
        harness.register();
        assert!(harness.dispatch("b1", "Ring").await.is_ok(), "seed {seed}");

        tokio::time::sleep(Duration::from_millis(10_500) - deleted_at).await;
        let status = harness.status("b1").await;
        if status != "Ringing" {
            violations.push(format!(
                "seed {seed} (deleted at {deleted_at:?}): the re-created record is {status}; the deleted tenant's \
                 timer fired into it"
            ));
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

/// Invariant: a follow-on scheduled by the deleted record does not fire into the record created again with the same
/// id, even though both incarnations scheduled it at the same event count and effect index.
#[tokio::test(start_paused = true)]
async fn dst_tenant_generation_follow_on_after_record_recreate() {
    let mut violations = Vec::new();
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let harness = Harness::new(seed);
        let old = harness.dispatch("b1", "Arm").await.expect("arm");
        let deleted_at = Duration::from_millis(1_000 + pick(seed, 6, 8_000));
        tokio::time::sleep(deleted_at).await;
        harness.delete().await;
        harness.register();
        let new = harness.dispatch("b1", "Arm").await.expect("arm again");
        assert_eq!(
            old.state.total_event_count, new.state.total_event_count,
            "seed {seed}: the re-created record counts its events from the start, so a follow-on key built from \
             (tenant, type, id, event count, effect index) repeats"
        );

        tokio::time::sleep(Duration::from_millis(10_500) - deleted_at).await;
        let status = harness.status("b1").await;
        if status != "Armed" {
            violations.push(format!(
                "seed {seed} (deleted at {deleted_at:?}): the re-created record is {status}; the deleted record's \
                 follow-on fired into it"
            ));
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

/// Invariant: a delete already under way when another delete of the same tenant finishes, and the tenant is created
/// again, leaves the tenant created again as it is: its specs and its records stay.
#[tokio::test(start_paused = true)]
async fn dst_tenant_generation_second_delete_after_recreate() {
    let mut violations = Vec::new();
    let mut raced = 0;
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let harness = Harness::new(seed);
        // A live actor, so both removals wait for it to end and the second is under way when the first finishes.
        assert!(harness.dispatch("b1", "Ring").await.is_ok(), "seed {seed}");

        let second_after = pick(seed, 7, 3);
        let first_storage = pick(seed, 8, 4);
        let second_storage = pick(seed, 9, 12);
        let tenant = acme();
        let second_started = std::cell::Cell::new(false);
        let created_while_second_ran = std::cell::Cell::new(false);
        let first_stamp = harness.state.tenant_generations.stamp(&tenant);
        let first = harness.state.remove_tenant(&first_stamp, || async {
            yields(first_storage).await;
            Ok(harness.remove_storage())
        });
        let second = async {
            yields(second_after).await;
            second_started.set(true);
            // `DELETE /api/tenants/acme` is admitted under the generation live when it arrives.
            let stamp = harness.state.tenant_generations.stamp(&tenant);
            harness
                .state
                .remove_tenant(&stamp, || async {
                    yields(second_storage).await;
                    Ok(harness.remove_storage())
                })
                .await
        };
        let creator = async {
            // `POST /api/tenants`, retried while the id is still being deleted.
            for _ in 0..64 {
                yields(1).await;
                if let Some(_lease) = harness.state.tenant_generations.lease_live(&tenant) {
                    harness.register();
                    created_while_second_ran.set(second_started.get());
                    break;
                }
            }
            harness.dispatch("b2", "Ring").await.is_ok()
        };
        let (first, second, created) = tokio::join!(first, second, creator);

        let schedule = format!(
            "second delete after {second_after} yields, storage after {first_storage} and {second_storage}"
        );
        if let Err(error) = first.and(second) {
            violations.push(format!(
                "seed {seed} ({schedule}): a removal did not finish: {error}"
            ));
            continue;
        }
        // A second delete that started after the tenant was created again deletes the new tenant, as it should.
        if !created_while_second_ran.get() {
            continue;
        }
        raced += 1;
        let specs = harness
            .state
            .registry
            .read()
            .expect("registry")
            .get_table(&tenant, "Bell")
            .is_some();
        let stored = harness
            .events
            .list_all_persistence_ids()
            .iter()
            .any(|id| id.starts_with("acme:") && id.ends_with(":b2"));
        if !(created && specs && stored) {
            violations.push(format!(
                "seed {seed} ({schedule}): the delete that was already under way removed the tenant created again \
                 (record created: {created}, specs kept: {specs}, record stored: {stored})"
            ));
        }
    }
    assert!(
        raced > 0,
        "no seed created the tenant again while the second delete ran"
    );
    assert!(violations.is_empty(), "{violations:#?}");
}
