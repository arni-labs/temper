//! Every commit that has answered is visible to a filtered read: a composite
//! batch's moves as much as a single move's (`project_before_dependents`).
//!
//! The workload mixes single moves and batches over a repository's refs, the
//! way Genesis pushes do, and after every answer reads the refs back with a
//! filter. The projection queue here never drains, the way a production queue
//! lags under load. On Genesis, a push's batch moved a Ref's head and answered
//! 200, and `Refs?$filter=RepositoryId eq ..` showed the old head for up to
//! 134 s: the filter still matched the stale row, so no empty-match re-check
//! ran.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::scheduler::{install_deterministic_context, sim_uuid};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use super::*;
use crate::state::QueryProjectionWriteQueue;
use crate::storage::StorageStack;

const SEEDS: u64 = 32;
const REFS: usize = 4;
const STEPS: usize = 12;

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

fn ref_id(n: usize) -> String {
    format!("rf-repo-test-refs-heads-b{n}")
}

/// The head ref `n` moves to at `step`.
fn sha(step: usize, n: usize) -> String {
    format!("{:040x}", step * REFS + n)
}

/// A seeded choice in `0..n`.
fn pick(n: usize) -> usize {
    (sim_uuid().as_u128() % n as u128) as usize
}

/// Each ref's head as `GET /tdata/Refs?$filter=RepositoryId eq 'repo-test'`
/// lists it, the way Genesis reads a repository's refs.
async fn listed_heads(state: &ServerState) -> BTreeMap<String, String> {
    let mut request = Request::get("/tdata/Refs?$filter=RepositoryId%20eq%20'repo-test'")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "batch-visible-test",
                "test-agent",
                None,
            ),
        ));
    let response = crate::router::build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    body["value"]
        .as_array()
        .expect("a collection answer")
        .iter()
        .map(|row| {
            (
                row["entity_id"].as_str().unwrap_or_default().to_string(),
                row["fields"]["TargetCommitSha"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn every_answered_commit_is_visible_to_a_filtered_read() {
    for seed in 0..SEEDS {
        let (_guard, _, _) = install_deterministic_context(seed);
        let (state, db_path) = lagging_state(seed).await;
        let tenant = TenantId::default();
        let agent = AgentContext::for_service("batch-visible");
        let mut heads = BTreeMap::new();
        for n in 0..REFS {
            let params = json!({
                "RepositoryId": "repo-test",
                "Name": format!("refs/heads/b{n}"),
                "TargetCommitSha": sha(0, n),
                "Kind": "branch"
            });
            let created = state
                .dispatch_tenant_action(&tenant, "Ref", &ref_id(n), "Create", params, &agent)
                .await
                .expect("ref create should run");
            assert!(created.success);
            heads.insert(ref_id(n), sha(0, n));
        }

        let mut trace = Vec::new();
        for step in 1..=STEPS {
            // A push moves one to all of the refs in one batch; a single
            // move moves one.
            let batch = pick(2) == 0;
            let count = if batch { 1 + pick(REFS) } else { 1 };
            let first = pick(REFS);
            let moved: Vec<usize> = (0..count).map(|k| (first + k) % REFS).collect();
            let update = |n: usize| {
                json!({
                    "PreviousCommitSha": heads[&ref_id(n)],
                    "NewCommitSha": sha(step, n),
                    "TargetCommitSha": sha(step, n)
                })
            };
            if batch {
                let sub_writes: Vec<Value> = moved
                    .iter()
                    .map(|&n| {
                        json!({
                            "entity_type": "Ref",
                            "entity_id": ref_id(n),
                            "action": "Update",
                            "params": update(n)
                        })
                    })
                    .collect();
                let applied = state
                    .apply_composite_integration_result(
                        &tenant,
                        "Parent",
                        "repo-test",
                        "IngestPack",
                        &json!({ "sub_writes": sub_writes }),
                        &agent,
                    )
                    .await
                    .expect("the push's batch commits");
                assert!(applied);
            } else {
                let n = moved[0];
                let response = state
                    .dispatch_tenant_action(&tenant, "Ref", &ref_id(n), "Update", update(n), &agent)
                    .await
                    .expect("the ref update should run");
                assert!(response.success);
            }
            for &n in &moved {
                heads.insert(ref_id(n), sha(step, n));
            }
            trace.push(format!(
                "{step}: {} {moved:?}",
                if batch { "batch" } else { "move" }
            ));

            assert_eq!(
                listed_heads(&state).await,
                heads,
                "seed {seed}: a filtered read must show every answered commit, after {trace:?}"
            );
        }
        let _ = std::fs::remove_file(db_path);
    }
}
