//! A move a composite batch commits is visible to a filtered read once the
//! batch answers, as a single move's is (`project_before_dependents`).
//!
//! The projection queue here never drains, the way a production queue lags
//! under load. On Genesis, a push's batch moved a Ref's head and answered
//! 200, and `Refs?$filter=RepositoryId eq ..` showed the old head for up to
//! 134 s: the filter still matched the stale row, so no empty-match
//! re-check ran.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::scheduler::{install_deterministic_context, sim_uuid};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use super::*;
use crate::state::QueryProjectionWriteQueue;
use crate::storage::StorageStack;

const SEEDS: u64 = 8;
const REF_ID: &str = "rf-repo-test-refs-heads-main";
const OLD_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_SHA: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// The composite fixture on a Turso store, with a projection queue that
/// never drains.
async fn lagging_state(seed: u64) -> (ServerState, std::path::PathBuf) {
    let db_path =
        std::env::temp_dir().join(format!("temper-batch-visible-{seed}-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let store = TursoEventStore::new(&format!("file:{}", db_path.display()), None)
        .await
        .expect("create local turso db");
    let mut specs = BTreeMap::new();
    for (name, ioa) in [("Parent", PARENT_IOA), ("Blob", BLOB_IOA), ("Ref", REF_IOA)] {
        specs.insert(name.to_string(), ioa.to_string());
    }
    let state = ServerState::with_storage_stack(
        ActorSystem::new("composite-batch-visible"),
        parse_csdl(COMPOSITE_CSDL).expect("test CSDL should parse"),
        COMPOSITE_CSDL.to_string(),
        specs,
        StorageStack::from_turso(store),
    )
    .expect("test state should build");
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy should parse");
    let query_plane = state.query_plane_store().expect("turso has a query plane");
    *state.query_projection_queue.lock().unwrap() = Some(Arc::new(
        QueryProjectionWriteQueue::new_for_test(query_plane, 32, 32),
    ));
    (state, db_path)
}

/// `GET /tdata/Refs?$filter=RepositoryId eq 'repo-test'`, as Genesis reads a
/// repository's refs.
async fn refs_of_repo(state: &ServerState) -> String {
    let response = crate::router::build_router(state.clone())
        .oneshot(
            Request::get("/tdata/Refs?$filter=RepositoryId%20eq%20'repo-test'")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

#[tokio::test]
async fn a_filtered_read_after_a_batch_answers_sees_the_batch() {
    for seed in 0..SEEDS {
        let (_guard, _, _) = install_deterministic_context(seed);
        let (state, db_path) = lagging_state(seed).await;
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("batch-visible");
        let created = state
            .dispatch_tenant_action(
                &tenant,
                "Ref",
                REF_ID,
                "Create",
                json!({
                    "RepositoryId": "repo-test",
                    "Name": "refs/heads/main",
                    "TargetCommitSha": OLD_SHA,
                    "Kind": "branch"
                }),
                &agent,
            )
            .await
            .expect("ref create should run");
        assert!(created.success);
        assert!(
            refs_of_repo(&state).await.contains(OLD_SHA),
            "seed {seed}: the ref is listed before the push"
        );

        state
            .apply_composite_integration_result(
                &tenant,
                "Parent",
                "repo-test",
                "IngestPack",
                &json!({
                    "sub_writes": [{
                        "entity_type": "Ref",
                        "entity_id": REF_ID,
                        "action": "Update",
                        "params": {
                            "PreviousCommitSha": OLD_SHA,
                            "NewCommitSha": NEW_SHA,
                            "TargetCommitSha": NEW_SHA
                        }
                    }]
                }),
                &agent,
            )
            .await
            .expect("the push's batch commits");

        let listed = refs_of_repo(&state).await;
        assert!(
            listed.contains(NEW_SHA) && !listed.contains(OLD_SHA),
            "seed {seed}: a filtered read after the batch answered must show the new head: {listed}"
        );
        let _ = std::fs::remove_file(db_path);
    }
}
