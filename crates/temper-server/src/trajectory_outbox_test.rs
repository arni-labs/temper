use super::*;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

pub(super) fn entry(id: &str) -> TrajectoryEntry {
    TrajectoryEntry {
        timestamp: "2026-04-28T00:00:00Z".to_string(),
        tenant: "tenant".to_string(),
        entity_type: "Session".to_string(),
        entity_id: id.to_string(),
        action: "ProgressMade".to_string(),
        success: true,
        from_status: Some("Running".to_string()),
        to_status: Some("Running".to_string()),
        error: None,
        agent_id: Some("agent".to_string()),
        session_id: Some("session".to_string()),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Entity),
        spec_governed: Some(true),
        agent_type: None,
        request_body: None,
        intent: None,
        matched_policy_ids: None,
        capture_seq: None,
    }
}

#[test]
fn try_record_reports_drop_when_full() {
    let outbox = TrajectoryOutbox::for_tests(1);
    assert!(outbox.try_record_for_test(entry("one")));
    assert!(!outbox.try_record_for_test(entry("two")));
    assert_eq!(outbox.dropped_total(), 1);
    assert_eq!(outbox.depth(), 1);
}

/// A sink whose every write fails, standing in for a backend that is down.
struct FailingSink;

#[async_trait::async_trait]
impl crate::storage::TrajectorySink for FailingSink {
    async fn persist_trajectory_entry(&self, _entry: &TrajectoryEntry) -> Result<(), String> {
        Err("backend unavailable".to_string())
    }
}

#[test]
fn a_session_is_marked_once() {
    // A backlog drops entries in bursts. One marker says the session's record
    // has holes; a thousand say the same thing to a backend that is already
    // the reason they are being dropped.
    release_capture_loss_marker("tenant", "burst-session");

    assert_eq!(
        claim_capture_loss_marker("tenant", "burst-session"),
        MarkerClaim::Claimed
    );
    assert_eq!(
        claim_capture_loss_marker("tenant", "burst-session"),
        MarkerClaim::AlreadyMarked
    );
    release_capture_loss_marker("tenant", "burst-session");
    assert_eq!(
        claim_capture_loss_marker("tenant", "burst-session"),
        MarkerClaim::Claimed,
        "a marker that failed to store leaves the session unmarked, so the next loss retries"
    );
    release_capture_loss_marker("tenant", "burst-session");
}

#[test]
fn a_full_dedupe_set_still_marks_the_session() {
    // The overflow behaviour that matters: past the cap the marker is written
    // without being remembered. Skipping it instead would make a lossy session
    // indistinguishable from a clean one, which is the whole failure the
    // marker exists to prevent.
    // On a set of its own, so filling it cannot make every other capture path
    // in the process look full.
    let mut marked = std::collections::BTreeSet::new();
    for index in 0..MAX_MARKED_SESSIONS {
        assert_eq!(
            claim_in(&mut marked, "tenant", &format!("session-{index}")),
            MarkerClaim::Claimed
        );
    }

    assert_eq!(
        claim_in(&mut marked, "tenant", "one-too-many"),
        MarkerClaim::Unremembered,
        "past the cap the marker is still written, only the memory of it is dropped"
    );
    assert_eq!(
        claim_in(&mut marked, "tenant", "one-too-many"),
        MarkerClaim::Unremembered,
        "an unremembered session must not later read as already marked"
    );
    assert_eq!(marked.len(), MAX_MARKED_SESSIONS, "the set stays bounded");
}

#[tokio::test]
async fn a_loss_interrupted_before_its_marker_lands_leaves_the_server_degraded() {
    // The process-death edge. The loss is counted before the write is even
    // queued, so every way the write can fail to happen — killed process,
    // cancelled runtime, ended simulation, worker that never ran — leaves the
    // count standing and the next conformance check degraded.
    let health = CaptureHealth::default();
    assert!(!health.is_degraded());

    assert!(health.record_unconfirmed_loss(None));

    assert!(
        health.is_degraded(),
        "a loss counted but not yet written must read as degraded, not clean"
    );
    assert_eq!(health.unconfirmed_losses(), 1);
    // Dropping everything here is the cancellation: nothing else runs.
}

#[tokio::test]
async fn a_marker_that_never_lands_leaves_the_loss_unconfirmed() {
    // Retries run on the worker rather than waiting for the next loss — for a
    // finished run there is no next loss — and when they are exhausted the
    // count simply stays up, because nothing durable will tell a reader this
    // session has a hole in it.
    release_capture_loss_marker("tenant", "doomed-session");
    let health = CaptureHealth::default();
    assert!(health.record_unconfirmed_loss(None));

    persist_capture_loss_marker(
        queued_marker_for_test(
            Arc::new(FailingSink),
            "tenant",
            "doomed-session",
            None,
            health.clone(),
        ),
        Duration::ZERO,
    )
    .await;

    assert_eq!(
        health.unconfirmed_losses(),
        1,
        "an unmarkable loss has to stay visible, and storage is not available"
    );
    assert_eq!(
        claim_capture_loss_marker("tenant", "doomed-session"),
        MarkerClaim::Claimed,
        "the exhausted session is released so a later loss in it tries again"
    );
    release_capture_loss_marker("tenant", "doomed-session");
}

#[tokio::test]
async fn only_a_confirmed_write_clears_the_loss() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_url = format!("file:{}", dir.path().join("marker-ok.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso store");
    let health = CaptureHealth::default();
    assert!(health.record_unconfirmed_loss(None));
    assert!(health.is_degraded());

    persist_capture_loss_marker(
        queued_marker_for_test(
            Arc::new(store),
            "tenant",
            "healthy-session",
            None,
            health.clone(),
        ),
        Duration::ZERO,
    )
    .await;

    assert!(
        !health.is_degraded(),
        "the marker is stored, so the loss it records is accounted for"
    );
}

#[test]
fn a_degraded_capture_stops_any_session_from_passing() {
    // A marker says which session lost a row. The degraded count says a row
    // was lost and could not be marked at all, so no session read from this
    // server can be assumed whole — including one that looks clean.
    let automaton = temper_spec::automaton::parse_automaton(include_str!(
        "../../../test-fixtures/specs/order.ioa.toml"
    ))
    .expect("order fixture parses");
    let rows = vec![temper_store_turso::TursoTrajectoryRow {
        tenant: "tenant".to_string(),
        entity_type: "Order".to_string(),
        entity_id: "order-1".to_string(),
        action: "AddItem".to_string(),
        success: true,
        from_status: Some("Draft".to_string()),
        to_status: Some("Draft".to_string()),
        error: None,
        agent_id: None,
        session_id: Some("session".to_string()),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some("Entity".to_string()),
        spec_governed: Some(true),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        request_body: None,
        intent: None,
        matched_policy_ids: None,
        capture_seq: None,
    }];

    let input = |capture_degraded| crate::conformance::ConformanceInput {
        automaton: &automaton,
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: false,
        spec_resolution: crate::conformance::SpecResolution::Pinned,
        capture_degraded,
    };

    assert!(
        crate::conformance::check_conformance(input(false)).passed,
        "the same run passes when the capture path is healthy"
    );
    let degraded = crate::conformance::check_conformance(input(true));
    assert!(!degraded.passed);
    assert!(!degraded.evidence_complete);
    assert!(
        degraded
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("could not record against any session")),
        "{:?}",
        degraded.evidence_gaps
    );
}

#[tokio::test]
async fn a_lost_entry_leaves_a_marker_the_checker_reads_as_missing_evidence() {
    // The end the finding cares about: after a loss, a conformance check
    // of that session must not come back `passed`.
    let dir = tempfile::tempdir().expect("create temp dir");
    let db_url = format!("file:{}", dir.path().join("capture-loss.db").display());
    let store = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso store");
    let session = "capture-loss-session";

    crate::storage::TrajectorySink::persist_trajectory_entry(
        &store,
        &TrajectoryEntry {
            session_id: Some(session.to_string()),
            ..entry("order-1")
        },
    )
    .await
    .expect("persist the entry that survived");

    persist_capture_loss_marker(
        queued_marker_for_test(
            Arc::new(store.clone()),
            "tenant",
            session,
            None,
            CaptureHealth::default(),
        ),
        Duration::ZERO,
    )
    .await;
    release_capture_loss_marker("tenant", session);

    let rows = store
        .query_trajectories_by_session(session, Some("tenant"), None, 10)
        .await
        .expect("read the session back");
    assert_eq!(rows.len(), 2, "the marker is stored alongside the run");

    let automaton = temper_spec::automaton::parse_automaton(include_str!(
        "../../../test-fixtures/specs/order.ioa.toml"
    ))
    .expect("order fixture parses");
    let report = crate::conformance::check_conformance(crate::conformance::ConformanceInput {
        automaton: &automaton,
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: false,
        spec_resolution: crate::conformance::SpecResolution::Pinned,
        capture_degraded: false,
    });

    assert_eq!(report.stats.capture_loss_markers, 1);
    assert!(
        !report.passed,
        "a run whose capture is known to have lost rows cannot pass: {:?}",
        report.evidence_gaps
    );
    assert!(!report.evidence_complete);
}

/// A sink that keeps what it is given, so a test can see whether a marker was written.
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

#[tokio::test]
async fn a_loss_of_a_deleted_tenant_is_cleared_and_the_tenant_created_again_starts_unmarked() {
    // The sessions a deleted tenant's markers would mark went with its storage, so no stored record is left
    // incomplete: its losses are accounted for, whether their marker was still queued or had exhausted its retries,
    // and the server does not read degraded for a tenant that is gone. Another tenant's loss still counts. The tenant
    // created again under the id starts with no session marked, so a loss in it is marked.
    let state = crate::state::ServerState::new(
        temper_runtime::ActorSystem::new("capture-loss-deleted-tenant"),
        temper_spec::csdl::CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    let health = state.capture_health.clone();
    let tenant = temper_runtime::tenant::TenantId::new("capture-loss-gone");
    let other = temper_runtime::tenant::TenantId::new("capture-loss-kept");
    let generation = state.generation_ref(&tenant);
    let sessions = ["lost-session", "exhausted-session", "marked-session"];
    for session in sessions {
        release_capture_loss_marker(tenant.as_str(), session);
        assert_eq!(
            claim_capture_loss_marker(tenant.as_str(), session),
            MarkerClaim::Claimed
        );
    }
    // `marked-session`'s marker was stored before the delete, `exhausted-session`'s ran out of retries, and
    // `lost-session`'s is still queued when the delete runs.
    assert!(health.record_unconfirmed_loss(Some(&generation)));
    assert!(health.record_unconfirmed_loss(Some(&generation)));
    persist_capture_loss_marker(
        queued_marker_for_test(
            Arc::new(FailingSink),
            tenant.as_str(),
            "exhausted-session",
            Some(generation.clone()),
            health.clone(),
        ),
        Duration::ZERO,
    )
    .await;
    let other_generation = state.generation_ref(&other);
    assert!(health.record_unconfirmed_loss(Some(&other_generation)));
    assert_eq!(health.unconfirmed_losses(), 3);

    state
        .remove_tenant(&state.tenant_generations.stamp(&tenant), || async {
            Ok(true)
        })
        .await
        .expect("removal finishes");
    let sink = Arc::new(RecordingSink::default());
    persist_capture_loss_marker(
        queued_marker_for_test(
            sink.clone(),
            tenant.as_str(),
            "lost-session",
            Some(generation),
            health.clone(),
        ),
        Duration::ZERO,
    )
    .await;

    assert!(
        sink.written.lock().expect("recording sink").is_empty(),
        "nothing is written into the tenant created again under the id"
    );
    assert_eq!(
        health.unconfirmed_losses(),
        1,
        "the deleted tenant's losses leave no stored session incomplete; the other tenant's loss still counts"
    );
    for session in sessions {
        assert_eq!(
            claim_capture_loss_marker(tenant.as_str(), session),
            MarkerClaim::Claimed,
            "a loss in session {session} of the tenant created again is marked"
        );
        release_capture_loss_marker(tenant.as_str(), session);
    }
}
