//! DST: a store that stops taking writes for a stretch, then takes them again.
//!
//! Workload, per seed: a few records take moves while the store takes writes,
//! then for a stretch it does not, then it does again. The stretch is drawn
//! from the seed: either every append fails in the store (a disk that answers
//! I/O errors) or the disk is under the floor (`STORAGE_FLOOR_BYTES`), so the
//! server refuses appends before writing. Moves go on throughout.
//!
//! Invariants:
//! - during the stretch every move answers `store_unavailable` and lands
//!   nothing; `/healthz` answers 503 while appends fail, and 200 with the
//!   refusal while the disk is under the floor; a new record whose first
//!   write the store does not take answers store unavailable, writes nothing
//!   and is not listed (its actor neither restarts nor stays failed);
//! - once the store takes writes again, every record moves again in the same
//!   server, with no restart, the new record is created and moves, and
//!   `/healthz` answers 200 once a write has landed;
//! - after every move, the record's state in memory is the state its journal
//!   gives.
//!
//! The server runs in one single-threaded, paused-clock Tokio runtime.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::CreateError;
use temper_server::storage::{STORAGE_FLOOR_BYTES, StorageRoom};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SEEDS: u64 = 48;
const RECORDS: usize = 3;
const TENANT: &str = "default";
/// A record first created during the stretch.
const LATE: &str = "late";

const TALLY: &str = r#"
[automaton]
name = "Tally"
states = ["Open", "Busy"]
initial = "Open"
allow_indefinite_states = ["Open", "Busy"]

[[state]]
name = "bumps"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Open", "Busy"]
effect = ["bumps += 1"]

[[action]]
name = "Hold"
kind = "input"
from = ["Open"]
to = "Busy"

[[action]]
name = "Release"
kind = "input"
from = ["Busy"]
to = "Open"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Tallies" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Tally"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Default"><EntitySet Name="Tallies" EntityType="Tallies.Tally"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// How the store stops taking writes for the stretch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stretch {
    /// Every append fails in the store.
    Failing,
    /// The disk has less than the floor available.
    UnderFloor(u64),
}

struct Run {
    seed: u64,
    trace: Vec<String>,
}

impl Run {
    fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }
}

fn journal_of(id: &str) -> String {
    format!("{TENANT}:Tally:{id}")
}

fn server(store: &SimEventStore, available: &Arc<AtomicU64>, seed: u64) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Tally", TALLY)]);
    let mut state =
        ServerState::from_registry(ActorSystem::new(format!("recovery-{seed}")), registry);
    let available = Arc::clone(available);
    let room = StorageRoom::read_with("/sim/disk", move |_| Ok(available.load(Ordering::SeqCst)));
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None).with_storage_room(room));
    state
}

/// `/healthz`: its status and body.
async fn healthz(state: &ServerState) -> (StatusCode, serde_json::Value) {
    let response = temper_server::write_health::healthz(State(state.clone())).await;
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the body reads");
    (
        status,
        serde_json::from_slice(&body).expect("the body is JSON"),
    )
}

/// How a move was answered.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Success,
    StoreUnavailable,
    Failed(String),
}

async fn move_once(state: &ServerState, id: &str, action: &str) -> Result<Answer, String> {
    let response = state
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "Tally",
            id,
            action,
            json!({}),
            &AgentContext::default(),
        )
        .await?;
    Ok(if response.success {
        Answer::Success
    } else if response.store_unavailable {
        Answer::StoreUnavailable
    } else {
        Answer::Failed(response.error.unwrap_or_default())
    })
}

/// The record's journal, replayed: (events, bumps, status).
async fn journal_state(store: &SimEventStore, id: &str) -> (usize, i64, String) {
    let events = store
        .read_events(&journal_of(id), 0)
        .await
        .expect("the journal reads");
    let mut bumps = 0;
    let mut status = "Open".to_string();
    for event in &events {
        match event.event_type.as_str() {
            "Bump" => bumps += 1,
            "Hold" => status = "Busy".to_string(),
            "Release" => status = "Open".to_string(),
            _ => {}
        }
    }
    (events.len(), bumps, status)
}

/// One move on `id`, checked against its journal; the record's status after it.
async fn checked_move(
    state: &ServerState,
    store: &SimEventStore,
    run: &mut Run,
    status: &mut BTreeMap<String, String>,
    id: &str,
    rng: &mut DeterministicRng,
) -> Result<(Answer, usize), String> {
    let action = match (status[id].as_str(), rng.next_bound(3)) {
        (_, 0) => "Bump",
        ("Open", _) => "Hold",
        _ => "Release",
    };
    let (before, _, _) = journal_state(store, id).await;
    let answer = move_once(state, id, action)
        .await
        .map_err(|error| run.fail("a move is answered", error))?;
    let (after, bumps, journal_status) = journal_state(store, id).await;
    run.trace.push(format!(
        "{action} {id}: {answer:?}, journal {before} -> {after}"
    ));
    let memory = state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Tally", id)
        .await
        .map_err(|error| run.fail("the record reads", error))?;
    let memory_bumps = memory.state.counters.get("bumps").copied().unwrap_or(0);
    if memory_bumps as i64 != bumps || memory.state.status != journal_status {
        return Err(run.fail(
            "the record's state in memory is the state its journal gives",
            format!(
                "{id}: memory bumps {memory_bumps} {}, journal bumps {bumps} {journal_status}",
                memory.state.status
            ),
        ));
    }
    status.insert(id.to_string(), journal_status);
    Ok((answer, after - before))
}

fn run_seed(seed: u64) -> Result<(), String> {
    let (_guard, _clock, _ids) = install_deterministic_context(seed);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime builds");
    let store = SimEventStore::no_faults(seed);
    let available = Arc::new(AtomicU64::new(u64::MAX));
    let mut rng = DeterministicRng::new(seed ^ 0x0057_07e5);
    let stretch = if rng.next_bound(2) == 0 {
        Stretch::Failing
    } else {
        Stretch::UnderFloor(rng.next_bound(STORAGE_FLOOR_BYTES as usize) as u64)
    };
    let mut run = Run {
        seed,
        trace: vec![format!("stretch {stretch:?}")],
    };
    runtime.block_on(async {
        let state = server(&store, &available, seed);
        let tenant = TenantId::new(TENANT);
        let mut status: BTreeMap<String, String> = BTreeMap::new();
        let ids: Vec<String> = (0..RECORDS).map(|i| format!("t-{i}")).collect();
        for id in &ids {
            state
                .get_or_create_tenant_entity(&tenant, "Tally", id, json!({ "Id": id }))
                .await
                .map_err(|error| run.fail("records are created", error.to_string()))?;
            status.insert(id.clone(), "Open".to_string());
        }

        for _ in 0..1 + rng.next_bound(3) {
            let id = ids[rng.next_bound(RECORDS)].clone();
            let (answer, landed) =
                checked_move(&state, &store, &mut run, &mut status, &id, &mut rng).await?;
            if answer != Answer::Success || landed != 1 {
                return Err(run.fail(
                    "moves land while the store takes writes",
                    format!("{answer:?}"),
                ));
            }
        }

        // The stretch.
        match stretch {
            Stretch::Failing => {
                for id in ids.iter().map(String::as_str).chain([LATE]) {
                    store.fail_next_appends(&journal_of(id), usize::MAX);
                }
            }
            Stretch::UnderFloor(bytes) => available.store(bytes, Ordering::SeqCst),
        }
        for _ in 0..2 + rng.next_bound(5) {
            let id = ids[rng.next_bound(RECORDS)].clone();
            let (answer, landed) =
                checked_move(&state, &store, &mut run, &mut status, &id, &mut rng).await?;
            if answer != Answer::StoreUnavailable || landed != 0 {
                return Err(run.fail(
                    "a move the store cannot take answers store_unavailable and lands nothing",
                    format!("{answer:?}, landed {landed}"),
                ));
            }
            let (code, body) = healthz(&state).await;
            let expected = match stretch {
                Stretch::Failing => StatusCode::SERVICE_UNAVAILABLE,
                Stretch::UnderFloor(_) => StatusCode::OK,
            };
            if code != expected || body.get("writes").is_none() {
                return Err(run.fail(
                    "/healthz says the store does not take writes",
                    format!("{code} {body}"),
                ));
            }
        }
        // A new record whose first write the store does not take is not
        // created: its creator is told, and nothing is left behind.
        let created = state
            .get_or_create_tenant_entity(&tenant, "Tally", LATE, json!({ "Id": LATE }))
            .await;
        let (events, _, _) = journal_state(&store, LATE).await;
        let listed = state.entity_exists(&tenant, "Tally", LATE);
        run.trace.push(format!(
            "create {LATE}: ok {}, journal {events}, listed {listed}",
            created.is_ok()
        ));
        match created {
            Err(CreateError::StoreUnavailable(_)) if events == 0 && !listed => {}
            other => {
                return Err(run.fail(
                    "a create the store does not take answers store unavailable and leaves no record",
                    format!(
                        "{:?}, journal {events}, listed {listed}",
                        other.map(|_| "created")
                    ),
                ));
            }
        }

        // The store takes writes again.
        match stretch {
            Stretch::Failing => {
                for id in ids.iter().map(String::as_str).chain([LATE]) {
                    store.fail_next_appends(&journal_of(id), 0);
                }
            }
            Stretch::UnderFloor(_) => available.store(
                STORAGE_FLOOR_BYTES + rng.next_bound(1 << 30) as u64,
                Ordering::SeqCst,
            ),
        }
        for id in &ids {
            let (answer, landed) =
                checked_move(&state, &store, &mut run, &mut status, id, &mut rng).await?;
            if answer != Answer::Success || landed != 1 {
                return Err(run.fail(
                    "every record moves again once the store takes writes, with no restart",
                    format!("{id}: {answer:?}, landed {landed}"),
                ));
            }
        }
        let created = state
            .get_or_create_tenant_entity(&tenant, "Tally", LATE, json!({ "Id": LATE }))
            .await;
        let (events, _, _) = journal_state(&store, LATE).await;
        if created.is_err() || events != 1 {
            return Err(run.fail(
                "the record the store did not take is created once it takes writes",
                format!("{:?}, journal {events}", created.map(|_| "created")),
            ));
        }
        status.insert(LATE.to_string(), "Open".to_string());
        let (answer, landed) =
            checked_move(&state, &store, &mut run, &mut status, LATE, &mut rng).await?;
        if answer != Answer::Success || landed != 1 {
            return Err(run.fail(
                "the record the store did not take moves once created",
                format!("{answer:?}, landed {landed}"),
            ));
        }
        let (code, body) = healthz(&state).await;
        if code != StatusCode::OK || body != json!({ "ok": true }) {
            return Err(run.fail(
                "/healthz is healthy once a write has landed",
                format!("{code} {body}"),
            ));
        }
        Ok(())
    })
}

#[test]
fn dst_a_store_that_stops_taking_writes_recovers_with_no_restart() {
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    for seed in 0..SEEDS {
        if let Err(report) = run_seed(seed) {
            let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
            let entry = broken
                .entry(invariant)
                .or_insert_with(|| (Vec::new(), report));
            entry.0.push(seed);
        }
    }
    let summary: Vec<String> = broken
        .iter()
        .map(|(invariant, (seeds, first))| format!("{invariant}: seeds {seeds:?}\nfirst: {first}"))
        .collect();
    assert!(
        broken.is_empty(),
        "invariants broken over {SEEDS} seeds:\n\n{}",
        summary.join("\n\n")
    );
}
