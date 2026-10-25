//! DST: a capture loss of a tenant generation is retired only by a stored marker or a deletion that finished.
//!
//! Each seed loses an entry of a tenant's session, then deletes the tenant while the loss's marker is still queued.
//! The deletion closes the generation first, so the marker's lease is refused, and then the deletion does not finish:
//! the seed picks whether its storage removal fails or its work is still running when the wait ends, and where the
//! marker runs against it. The session's storage is still there without its marker, so the loss must still be
//! counted. The deletion is then run again and finishes, which retires the loss with the storage.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;

use super::{
    CaptureHealth, MarkerClaim, claim_capture_loss_marker, persist_capture_loss_marker,
    queued_marker_for_test, release_capture_loss_marker,
};
use crate::state::trajectory::TrajectoryEntry;

const SEEDS: u64 = 32;

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

/// A sink that keeps what it is given, so the test can see whether a marker was written.
#[derive(Default)]
struct RecordingSink {
    written: std::sync::Mutex<Vec<TrajectoryEntry>>,
}

#[async_trait::async_trait]
impl crate::storage::TrajectorySink for RecordingSink {
    async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        self.written
            .lock()
            .expect("recording sink")
            .push(entry.clone());
        Ok(())
    }
}

fn state(seed: u64) -> crate::state::ServerState {
    let mut state = crate::state::ServerState::new(
        temper_runtime::ActorSystem::new(format!("capture-loss-deletion-{seed}")),
        temper_spec::csdl::CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    state.action_dispatch_timeout = Duration::from_millis(50);
    state
}

/// Invariant: while a tenant's deletion has not finished, a loss of its generation stays counted, even once the
/// loss's marker was refused by the closed generation; the deletion that finishes retires it, and a loss of the
/// deleted generation is not counted afterwards.
#[tokio::test(start_paused = true)]
async fn dst_capture_loss_survives_an_unfinished_deletion() {
    let mut violations = Vec::new();
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let state = state(seed);
        let health: CaptureHealth = state.capture_health.clone();
        let tenant = TenantId::new(format!("capture-loss-deletion-{seed}"));
        let session = format!("session-{seed}");
        let generation = state.generation_ref(&tenant);
        let stamp = generation.stamp().clone();

        // An entry of the session was lost: the session is claimed for its marker and the loss counted.
        release_capture_loss_marker(tenant.as_str(), &session);
        assert_eq!(
            claim_capture_loss_marker(tenant.as_str(), &session),
            MarkerClaim::Claimed,
            "seed {seed}"
        );
        assert!(
            health.record_unconfirmed_loss(Some(&generation)),
            "seed {seed}"
        );

        // How the deletion stops short: its storage removal fails, or work of the tenant is still running when its
        // wait ends, so its storage is kept.
        let still_running = pick(seed, 1, 2) == 1;
        let held = still_running.then(|| {
            state
                .tenant_generations
                .lease(&stamp)
                .expect("the live generation admits work")
        });
        let storage_after = pick(seed, 2, 6);
        let marker_after = 1 + pick(seed, 3, 10);
        let sink = Arc::new(RecordingSink::default());

        let removal = state.remove_tenant(&stamp, || async {
            yields(storage_after).await;
            Err(PersistenceError::Storage("store unavailable".to_string()))
        });
        let marker = async {
            yields(marker_after).await;
            persist_capture_loss_marker(
                queued_marker_for_test(
                    sink.clone(),
                    tenant.as_str(),
                    &session,
                    Some(generation.clone()),
                    health.clone(),
                ),
                Duration::ZERO,
            )
            .await;
        };
        let (removed, ()) = tokio::join!(removal, marker);
        let schedule = format!(
            "{}, marker after {marker_after} yields, storage after {storage_after}",
            if still_running {
                "work still running"
            } else {
                "storage removal fails"
            }
        );

        if removed.is_ok() {
            violations.push(format!("seed {seed} ({schedule}): the deletion finished"));
        }
        if !sink.written.lock().expect("recording sink").is_empty() {
            violations.push(format!(
                "seed {seed} ({schedule}): the marker was written through a closed generation"
            ));
        }
        let unfinished = health.unconfirmed_losses();
        if unfinished != 1 {
            violations.push(format!(
                "seed {seed} ({schedule}): {unfinished} losses counted after the deletion did not finish; the \
                 session is still stored without its marker, so the loss must still count"
            ));
        }

        // The deletion is run again and finishes: the session went with the storage, which retires the loss.
        drop(held);
        let retried = state.remove_tenant(&stamp, || async { Ok(true) }).await;
        if !matches!(retried, Ok(true)) {
            violations.push(format!(
                "seed {seed} ({schedule}): the retried deletion did not finish: {retried:?}"
            ));
        }
        let finished = health.unconfirmed_losses();
        if finished != 0 {
            violations.push(format!(
                "seed {seed} ({schedule}): {finished} losses counted after the deletion finished"
            ));
        }
        if health.record_unconfirmed_loss(Some(&generation)) || health.unconfirmed_losses() != 0 {
            violations.push(format!(
                "seed {seed} ({schedule}): a loss of the deleted generation was counted after its deletion"
            ));
        }
        release_capture_loss_marker(tenant.as_str(), &session);
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

/// An ordinary queued row refused during an unfinished deletion must degrade capture health,
/// just as a failed write does. A completed deletion retires it, and late old rows stay retired.
#[tokio::test(start_paused = true)]
async fn dst_queued_entry_refused_during_deletion_records_capture_loss() {
    for seed in 0..SEEDS {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let state = state(seed);
        let tenant = TenantId::new(format!("queued-loss-deletion-{seed}"));
        let generation = state.generation_ref(&tenant);
        let stamp = generation.stamp().clone();
        let sink = Arc::new(RecordingSink::default());
        let mut entry = super::tests::entry("entry");
        entry.tenant = tenant.to_string();
        entry.session_id = Some(format!("queued-loss-session-{seed}"));
        let queued = || super::QueuedTrajectory {
            sink: Some(sink.clone()),
            backend: "test",
            entry: entry.clone(),
            generation: Some(generation.clone()),
            health: state.capture_health.clone(),
            parent: opentelemetry::Context::new(),
        };
        // The seed chooses a failed storage deletion or a timeout waiting for admitted work.
        let held = (pick(seed, 1, 2) == 1).then(|| state.tenant_generations.lease(&stamp).unwrap());
        assert!(
            state
                .remove_tenant(&stamp, || async {
                    Err(PersistenceError::Storage("unavailable".into()))
                })
                .await
                .is_err()
        );
        super::persist_drained(queued()).await;
        assert_eq!(
            state.capture_health.unconfirmed_losses(),
            1,
            "seed {seed}: refused ordinary row must count"
        );
        assert!(
            sink.written.lock().unwrap().is_empty(),
            "seed {seed}: closed generation cannot write"
        );
        drop(held);
        assert!(
            state
                .remove_tenant(&stamp, || async { Ok(true) })
                .await
                .unwrap()
        );
        assert_eq!(
            state.capture_health.unconfirmed_losses(),
            0,
            "seed {seed}: deletion retires loss"
        );
        super::persist_drained(queued()).await;
        assert_eq!(
            state.capture_health.unconfirmed_losses(),
            0,
            "seed {seed}: late deleted row stays retired"
        );
    }
}
