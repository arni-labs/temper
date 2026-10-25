//! DST: a move's answer is what its commit did.
//!
//! Workload, per seed: a few records take a stream of moves. Before each
//! move the store is set, from the seed, to do one of: write it; fail before
//! writing; write it and answer with a storage error (a commit that landed
//! while its caller was told it failed); write it and answer with a
//! concurrency violation (a store that retried a write whose answer was
//! lost); conflict before writing; write it late, past the dispatch timeout;
//! or write it, fail the answer and fail the next journal read too, so the
//! outcome cannot be learned. Some callers hang up: their dispatch is dropped
//! after a seeded number of scheduler turns.
//!
//! Invariants, after every move:
//! - a move lands at most once;
//! - a successful answer is a move that landed;
//! - a failed answer is a move that did not land, unless the journal could
//!   not be read when it was answered;
//! - the record's state in memory is the state its journal gives, after any
//!   answer whose outcome could be learned.
//!
//! The server runs in one single-threaded, paused-clock Tokio runtime.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicRng, install_deterministic_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const SEEDS: u64 = 64;
const RECORDS: usize = 3;
const MOVES: usize = 24;
const TENANT: &str = "default";

const TALLY: &str = r#"
[automaton]
name = "Tally"
states = ["Open", "Busy"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[key]]
name = "code"
properties = ["Code"]

[[state]]
name = "bumps"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Open", "Busy"]
params = ["Mark"]
effect = ["bumps += 1"]

[[action]]
name = "Hold"
kind = "input"
from = ["Open"]
to = "Busy"
params = ["Mark"]

[[action]]
name = "Release"
kind = "input"
from = ["Busy"]
to = "Open"
params = ["Mark"]

[[action]]
name = "Expire"
kind = "internal"
from = ["Busy"]
to = "Open"

[[state_timeout]]
state = "Busy"
after_seconds = 86400
on_timeout = "Expire"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Tallies" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Tally"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="Code" Type="Edm.String"/>
<Property Name="Mark" Type="Edm.String"/>
</EntityType>
<EntityContainer Name="Default"><EntitySet Name="Tallies" EntityType="Tallies.Tally"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// What the store does with a move's commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    FailBeforeWriting,
    LandThenFail,
    LandThenConflict,
    ConflictBeforeWriting,
    LandLate,
    LandThenFailUnreadable,
}

impl Fault {
    fn draw(rng: &mut DeterministicRng) -> Self {
        match rng.next_bound(10) {
            0 => Self::FailBeforeWriting,
            1 => Self::LandThenFail,
            2 => Self::LandThenConflict,
            3 => Self::ConflictBeforeWriting,
            4 => Self::LandLate,
            5 => Self::LandThenFailUnreadable,
            _ => Self::None,
        }
    }

    fn apply(self, store: &SimEventStore, journal: &str) {
        match self {
            Self::None => {}
            Self::FailBeforeWriting => store.fail_next_appends(journal, 1),
            Self::LandThenFail => store.land_next_appends_then_fail(journal, 1),
            Self::LandThenConflict => store.lose_next_append_answers(journal, 1),
            Self::ConflictBeforeWriting => store.inject_concurrency_violations(journal, 1),
            Self::LandLate => store.inject_append_delay(journal, Duration::from_millis(40)),
            Self::LandThenFailUnreadable => {
                store.land_next_appends_then_fail(journal, 1);
                store.fail_next_reads(journal, 1);
            }
        }
    }

    /// Clear what the move did not use up, so it cannot reach the next move.
    fn clear(store: &SimEventStore, journal: &str) {
        store.fail_next_appends(journal, 0);
        store.land_next_appends_then_fail(journal, 0);
        store.lose_next_append_answers(journal, 0);
        store.inject_concurrency_violations(journal, 0);
        store.fail_next_reads(journal, 0);
    }
}

/// How a move was answered.
#[derive(Debug)]
enum Answer {
    Success,
    Refused(String),
    Failed(String),
    #[expect(dead_code, reason = "read through Debug, in the trace")]
    DispatchError(String),
    HungUp,
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

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

fn server(store: &SimEventStore, seed: u64) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Tally", TALLY)]);
    let mut state =
        ServerState::from_registry(ActorSystem::new(format!("commit-{seed}")), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state.action_dispatch_timeout = Duration::from_millis(25);
    state
}

/// How many of the journal's events carry `mark`.
async fn landed(store: &SimEventStore, id: &str, mark: &str) -> usize {
    store
        .read_events(&journal_of(id), 0)
        .await
        .expect("the journal reads")
        .iter()
        .filter(|event| event.payload["params"]["Mark"] == json!(mark))
        .count()
}

async fn move_once(
    state: &ServerState,
    id: &str,
    action: &str,
    mark: &str,
    hang_up_after: Option<usize>,
) -> Answer {
    let dispatch = {
        let state = state.clone();
        let id = id.to_string();
        let action = action.to_string();
        let params = json!({ "Mark": mark });
        async move {
            state
                .dispatch_tenant_action(
                    &TenantId::new(TENANT),
                    "Tally",
                    &id,
                    &action,
                    params,
                    &AgentContext::default(),
                )
                .await
        }
    };
    let result = match hang_up_after {
        None => dispatch.await,
        Some(turns) => {
            let handle = tokio::spawn(dispatch); // determinism-ok: the caller, on this runtime's one thread
            for _ in 0..turns {
                tokio::task::yield_now().await;
            }
            if !handle.is_finished() {
                handle.abort();
                return Answer::HungUp;
            }
            handle.await.expect("the dispatch task ends")
        }
    };
    match result {
        Ok(response) if response.success => Answer::Success,
        Ok(response) if response.refused => Answer::Refused(response.error.unwrap_or_default()),
        Ok(response) => Answer::Failed(response.error.unwrap_or_default()),
        Err(error) => Answer::DispatchError(error),
    }
}

/// The record's journal, replayed: (sequence, bumps, status).
async fn journal_state(store: &SimEventStore, id: &str) -> (u64, i64, String) {
    let events = store
        .read_events(&journal_of(id), 0)
        .await
        .expect("the journal reads");
    let sequence = events.last().map(|event| event.sequence_nr).unwrap_or(0);
    let mut bumps = 0;
    let mut status = "Open".to_string();
    for event in &events {
        match event.event_type.as_str() {
            "Bump" => bumps += 1,
            "Hold" => status = "Busy".to_string(),
            "Release" | "Expire" => status = "Open".to_string(),
            _ => {}
        }
    }
    (sequence, bumps, status)
}

fn run_seed(seed: u64) -> Result<usize, String> {
    let (_guard, _clock, _ids) = install_deterministic_context(seed);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime builds");
    let store = SimEventStore::no_faults(seed);
    let mut rng = DeterministicRng::new(seed ^ 0xc0_33_17);
    let mut run = Run {
        seed,
        trace: Vec::new(),
    };
    runtime.block_on(async {
        let state = server(&store, seed);
        let tenant = TenantId::new(TENANT);
        let mut status: BTreeMap<String, String> = BTreeMap::new();
        for i in 0..RECORDS {
            let id = format!("t-{i}");
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Tally",
                    &id,
                    json!({ "Id": id, "Code": format!("code-{i}") }),
                )
                .await
                .map_err(|error| run.fail("records are created", error))?;
            status.insert(id, "Open".to_string());
        }
        let mut faulted = 0;
        for n in 0..MOVES {
            let id = format!("t-{}", rng.next_bound(RECORDS));
            let journal = journal_of(&id);
            let action = match (status[&id].as_str(), rng.next_bound(3)) {
                (_, 0) => "Bump",
                ("Open", _) => "Hold",
                _ => "Release",
            };
            let mark = format!("m-{n}");
            let fault = Fault::draw(&mut rng);
            let hang_up_after = (rng.next_bound(5) == 0).then(|| rng.next_bound(40));
            fault.apply(&store, &journal);
            faulted += usize::from(fault != Fault::None);

            let answer = move_once(&state, &id, action, &mark, hang_up_after).await;
            // A hung-up move's actor keeps going; let its late writes land.
            tokio::time::sleep(Duration::from_secs(1)).await;
            settle().await;
            Fault::clear(&store, &journal);
            run.trace.push(format!(
                "{mark}: {action} {id}, {fault:?}, hang up {hang_up_after:?}: {answer:?}"
            ));

            let landings = landed(&store, &id, &mark).await;
            if landings > 1 {
                return Err(run.fail(
                    "a move lands at most once",
                    format!("{mark} landed {landings} times"),
                ));
            }
            match &answer {
                Answer::Success if landings != 1 => {
                    return Err(run.fail(
                        "a successful answer is a move that landed",
                        format!("{mark} answered success, landed {landings} times"),
                    ));
                }
                Answer::Refused(error) | Answer::Failed(error) if landings != 0 => {
                    let unreadable = fault == Fault::LandThenFailUnreadable;
                    if !unreadable {
                        return Err(run.fail(
                            "a failed answer is a move that did not land",
                            format!("{mark} answered {error:?} and landed"),
                        ));
                    }
                }
                _ => {}
            }

            // The outcome could be learned unless the journal read failed.
            if fault != Fault::LandThenFailUnreadable {
                let (sequence, bumps, journal_status) = journal_state(&store, &id).await;
                let memory = state
                    .get_tenant_entity_state(&tenant, "Tally", &id)
                    .await
                    .map_err(|error| run.fail("the record reads", error))?;
                let memory_bumps = memory.state.counters.get("bumps").copied().unwrap_or(0);
                if memory.state.sequence_nr != sequence
                    || memory_bumps as i64 != bumps
                    || memory.state.status != journal_status
                {
                    return Err(run.fail(
                        "the record's state in memory is the state its journal gives",
                        format!(
                            "{id} after {mark}: memory seq {} bumps {memory_bumps} {}, journal seq {sequence} bumps {bumps} {journal_status}",
                            memory.state.sequence_nr, memory.state.status
                        ),
                    ));
                }
                status.insert(id.clone(), journal_status);
            } else {
                let (_, _, journal_status) = journal_state(&store, &id).await;
                status.insert(id.clone(), journal_status);
            }
        }
        Ok(faulted)
    })
}

#[test]
fn dst_a_moves_answer_is_what_its_commit_did() {
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    let mut faulted = 0;
    for seed in 0..SEEDS {
        match run_seed(seed) {
            Ok(moves) => faulted += moves,
            Err(report) => {
                let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
                let entry = broken
                    .entry(invariant)
                    .or_insert_with(|| (Vec::new(), report));
                entry.0.push(seed);
            }
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
    // A harness that never faults a commit cannot catch a wrong answer.
    assert!(faulted > 0, "no move over {SEEDS} seeds met a fault");
}
