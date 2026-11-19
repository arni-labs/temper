//! DST: a `$batch` of moves does what the same moves sent one by one do.
//!
//! Two servers, each on its own simulated store, take the same workload per
//! seed. The singles server is sent every row as its own request; the batch
//! server is sent the rows of each round as one `POST /tdata/$batch`, while
//! single writers move the round's other records at the same time. Some
//! batches are sent, hung up on part way, and sent again with the same body
//! and keys.
//!
//! Invariants, per seed:
//! - every row's answer has the status its single request had, in the rows'
//!   order, and no row comes back `NotStarted`;
//! - after every round each record's journal holds the same moves on both
//!   servers, so a batch applies each row once, reactions included;
//! - a batch hung up on and sent again applies each row exactly once, and
//!   its second answer per row is the single request's.
//!
//! Each server runs in one single-threaded, paused-clock Tokio runtime.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicIdGen, DeterministicRng, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const SEEDS: u64 = 24;
const ROUNDS: usize = 5;
const RECORDS: usize = 6;
const TENANT: &str = "default";

/// Bump is refused once a record has three bumps; Ping's reaction bumps the
/// same record, so a row's reactions are part of what it does.
const TALLY: &str = r#"
[automaton]
name = "Tally"
states = ["Open", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "Closed"]

[[state]]
name = "bumps"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Open"]
guard = "bumps < 3"
effect = ["bumps += 1"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "Ping"
kind = "input"
from = ["Open", "Closed"]
[[action.triggers]]
name = "bump"
kind = "entity"
target_entity = "Tally"
resolve_target = { kind = "same_id" }
target_action = "Bump"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Tallies" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Tally"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Bump" IsBound="true"><Parameter Name="bindingParameter" Type="Tallies.Tally"/></Action>
<Action Name="Close" IsBound="true"><Parameter Name="bindingParameter" Type="Tallies.Tally"/></Action>
<Action Name="Ping" IsBound="true"><Parameter Name="bindingParameter" Type="Tallies.Tally"/></Action>
<EntityContainer Name="Default"><EntitySet Name="Tallies" EntityType="Tallies.Tally"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// One write: where it goes, its body and its idempotency key.
#[derive(Debug, Clone)]
struct Row {
    id: String,
    url: String,
    body: Value,
    key: String,
}

fn server(store: &SimEventStore, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Tally", TALLY)]);
    registry.set_verification_status(
        &TenantId::new(TENANT),
        "Tally",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-11-05T00:00:00Z".into(),
        }),
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("the policy parses");
    state.rebuild_reaction_dispatcher();
    state
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("the runtime builds")
}

async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}

fn post(uri: &str, body: &Value, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(key) = key {
        builder = builder.header("idempotency-key", key);
    }
    let mut request = builder
        .body(Body::from(body.to_string()))
        .expect("the request builds");
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            SecurityContext::from_resolved_identity("ann", "person", None),
        ));
    request
}

async fn send(state: &ServerState, request: Request<Body>) -> (u16, Value) {
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the body reads");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn single(state: &ServerState, row: &Row) -> u16 {
    send(
        state,
        post(&format!("/tdata/{}", row.url), &row.body, Some(&row.key)),
    )
    .await
    .0
}

fn batch_body(rows: &[Row]) -> Value {
    json!({ "requests": rows.iter().map(|row| json!({
        "id": row.id,
        "method": "POST",
        "url": row.url,
        "headers": { "Idempotency-Key": row.key },
        "body": row.body,
    })).collect::<Vec<_>>() })
}

/// Each row's status in a batch's answer, in order.
async fn batch(state: &ServerState, rows: &[Row], seed: u64) -> Vec<u16> {
    let (status, answer) = send(state, post("/tdata/$batch", &batch_body(rows), None)).await;
    assert_eq!(status, 200, "seed {seed}: the batch is answered: {answer}");
    let answers = answer["responses"].as_array().expect("responses");
    assert_eq!(answers.len(), rows.len(), "seed {seed}: one answer per row");
    answers
        .iter()
        .zip(rows)
        .map(|(answer, row)| {
            assert_eq!(
                answer["id"],
                row.id.as_str(),
                "seed {seed}: rows answer in order"
            );
            assert_ne!(
                answer["body"]["error"]["code"], "NotStarted",
                "seed {seed}: no row lacks time: {answer}"
            );
            assert_eq!(answer["headers"]["idempotency-key"], row.key.as_str());
            answer["status"].as_u64().expect("a status") as u16
        })
        .collect()
}

async fn journals(store: &SimEventStore, ids: &[String]) -> BTreeMap<String, Vec<String>> {
    let mut journals = BTreeMap::new();
    for id in ids {
        let events = store
            .read_events(&format!("{TENANT}:Tally:{id}"), 0)
            .await
            .expect("the journal reads");
        journals.insert(
            id.clone(),
            events.into_iter().map(|e| e.event_type).collect(),
        );
    }
    journals
}

/// A round: a batch over some records, single writers on the others.
fn draw_round(rng: &mut DeterministicRng, ids: &[String], round: usize) -> (Vec<Row>, Vec<Row>) {
    let mut shuffled = ids.to_vec();
    for i in (1..shuffled.len()).rev() {
        shuffled.swap(i, rng.next_bound(i + 1));
    }
    let in_batch = 1 + rng.next_bound(shuffled.len());
    let mut row = |id: &str, n: usize| {
        let action = ["Bump", "Bump", "Close", "Ping"][rng.next_bound(4)];
        Row {
            id: format!("r{round}-{n}"),
            url: format!("Tallies('{id}')/Tallies.{action}"),
            body: json!({}),
            key: format!("k{round}-{n}-{id}"),
        }
    };
    let batch: Vec<Row> = shuffled[..in_batch]
        .iter()
        .enumerate()
        .map(|(n, id)| row(id, n))
        .collect();
    let singles: Vec<Row> = shuffled[in_batch..]
        .iter()
        .enumerate()
        .map(|(n, id)| row(id, 100 + n))
        .collect();
    (batch, singles)
}

#[test]
fn dst_a_batch_does_what_its_rows_sent_singly_do() {
    for seed in 0..SEEDS {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let singles_store = SimEventStore::no_faults(seed);
        let batch_store = SimEventStore::no_faults(seed);
        let mut rng = DeterministicRng::new(seed);
        runtime().block_on(async {
            clock.begin();
            let singles_server = server(&singles_store, &format!("singles-{seed}"));
            let batch_server = server(&batch_store, &format!("batch-{seed}"));
            let ids: Vec<String> = (0..RECORDS).map(|i| format!("t{i}")).collect();
            // The records are made on both servers, the batch server's by
            // one batch of creates.
            let creates: Vec<Row> = ids
                .iter()
                .map(|id| Row {
                    id: format!("make-{id}"),
                    url: "Tallies".into(),
                    body: json!({ "Id": id }),
                    key: format!("make-{id}"),
                })
                .collect();
            let mut expected = Vec::new();
            for row in &creates {
                expected.push(single(&singles_server, row).await);
            }
            assert_eq!(batch(&batch_server, &creates, seed).await, expected, "seed {seed}: creates");

            for round in 0..ROUNDS {
                let (rows, others) = draw_round(&mut rng, &ids, round);
                let mut expected = Vec::new();
                for row in &rows {
                    expected.push(single(&singles_server, row).await);
                }
                for row in &others {
                    single(&singles_server, row).await;
                }
                let hang_up = rng.next_bound(3) == 0;
                if hang_up {
                    // The caller hangs up part way; the rows it reached
                    // must not be applied twice when it sends them again.
                    let request = post("/tdata/$batch", &batch_body(&rows), None);
                    let app = build_router(batch_server.clone());
                    let sent = tokio::spawn(app.oneshot(request));
                    for _ in 0..rng.next_bound(200) {
                        tokio::task::yield_now().await;
                    }
                    sent.abort();
                    settle().await;
                }
                let others_sent = async {
                    for row in &others {
                        tokio::task::yield_now().await;
                        single(&batch_server, row).await;
                    }
                };
                let (answered, ()) = tokio::join!(batch(&batch_server, &rows, seed), others_sent);
                settle().await;
                assert_eq!(
                    answered, expected,
                    "seed {seed} round {round} (hang-up {hang_up}): each row answers as its single request did\nrows: {rows:?}"
                );
                assert_eq!(
                    journals(&batch_store, &ids).await,
                    journals(&singles_store, &ids).await,
                    "seed {seed} round {round} (hang-up {hang_up}): the batch applied each row once\nrows: {rows:?}\nothers: {others:?}"
                );
            }
            clock.end(std::time::Duration::ZERO);
        });
    }
}

#[test]
fn dst_a_malformed_batch_runs_none_of_its_rows() {
    let clock = Arc::new(process_clock::ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(7)));
    let store = SimEventStore::no_faults(7);
    runtime().block_on(async {
        clock.begin();
        let state = server(&store, "malformed");
        let make = post("/tdata/Tallies", &json!({ "Id": "t0" }), Some("make-t0"));
        let (made, answer) = send(&state, make).await;
        assert!((200..300).contains(&made), "the record is made: {answer}");
        settle().await;
        let before = journals(&store, &["t0".to_string()]).await;
        let rows = [
            Row {
                id: "a".into(),
                url: "Tallies('t0')/Tallies.Bump".into(),
                body: json!({}),
                key: "a".into(),
            },
            Row {
                id: "b".into(),
                url: "Tallies('t0')/Tallies.Close".into(),
                body: json!({}),
                key: "b".into(),
            },
        ];
        let (status, answer) = send(&state, post("/tdata/$batch", &batch_body(&rows), None)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST.as_u16(), "{answer}");
        assert_eq!(answer["error"]["code"], "InvalidBatch");
        settle().await;
        assert_eq!(
            journals(&store, &["t0".to_string()]).await,
            before,
            "no row of a refused batch ran"
        );
        clock.end(std::time::Duration::ZERO);
    });
}
