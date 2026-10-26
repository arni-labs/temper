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
//! after a seeded number of scheduler turns, or the moment its commit lands,
//! while the server writes the state timer's row (a slow store step: on the
//! 25 Oct production trace a timer row took 7.5-13 s, and the site gave up
//! after 20 s).
//!
//! A `Hold` schedules a `Note` an hour later and, through a reaction, counts
//! itself on the record's `Ledger`.
//!
//! Invariants, after every move:
//! - a move lands at most once;
//! - a successful answer is a move that landed;
//! - a failed answer is a move that did not land, unless the journal could
//!   not be read when it was answered;
//! - the record's state in memory is the state its journal gives: when a
//!   write's outcome could not be learned, the record rebuilds from its
//!   journal before its next answer;
//! - a move takes one commit;
//! - the record's query-plane row and its timer row are what its journal
//!   says (`rows::rows_follow_journal`), whatever the answer and whether
//!   the caller hung up.
//!
//! And once the moves are done, a day later, whoever hung up:
//! - every timer has fired, so no record is still `Busy`;
//! - every `Hold` that landed has had its scheduled `Note` and its `Ledger`
//!   count, except one whose outcome could not be learned (its caller was
//!   answered "outcome unknown", so nothing ran after it; see `unknown`).
//!
//! The server runs in one single-threaded, paused-clock Tokio runtime.

#[path = "dst_commit_outcome/faults.rs"]
mod faults;
#[path = "dst_commit_outcome/reset_recovery.rs"]
mod reset_recovery;
#[path = "dst_commit_outcome/rows.rs"]
mod rows;
#[path = "dst_commit_outcome/specs.rs"]
mod specs;

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

use faults::Fault;
use specs::{CSDL, LEDGER, TALLY};

const SEEDS: u64 = 64;
const RECORDS: usize = 3;
const MOVES: usize = 24;
const TENANT: &str = "default";

/// When a caller hangs up.
#[derive(Debug, Clone, Copy)]
enum HangUp {
    /// After this many scheduler turns.
    AfterTurns(usize),
    /// The moment the move's commit lands.
    AfterCommit,
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
    /// Broken invariants the run goes on past, so the ones after them are
    /// checked too: the first break of each.
    noted: BTreeMap<&'static str, String>,
}

impl Run {
    fn fail(&self, invariant: &str, detail: String) -> String {
        format!(
            "seed {}: {invariant}: {detail}\ntrace:\n  {}",
            self.seed,
            self.trace.join("\n  ")
        )
    }

    /// Note a broken invariant and go on.
    fn note(&mut self, invariant: &'static str, detail: String) {
        if !self.noted.contains_key(invariant) {
            let report = self.fail(invariant, detail);
            self.noted.insert(invariant, report);
        }
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
    server_with_tally(store, seed, TALLY)
}

fn server_with_tally(store: &SimEventStore, seed: u64, tally: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(
        TENANT,
        csdl,
        CSDL.to_string(),
        &[("Tally", tally), ("Ledger", LEDGER)],
    );
    let mut state =
        ServerState::from_registry(ActorSystem::new(format!("commit-{seed}")), registry);
    // The sim store keeps each record's rows itself, and the server reads its
    // timer rows through it: its commits write the rows this server reads.
    state.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal(),
    );
    state.state_timer_rows = Some(std::sync::Arc::new(rows::SimTimerRows(store.clone())));
    state.action_dispatch_timeout = Duration::from_millis(25);
    // A reaction is authorized as its own move: with no tenant policy, Cedar
    // refuses every `Ledger.Count`, and no hold could ever be counted.
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("the tenant policy parses");
    state.rebuild_reaction_dispatcher();
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
    store: &SimEventStore,
    id: &str,
    action: &str,
    mark: &str,
    hang_up: Option<HangUp>,
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
    let result = match hang_up {
        None => dispatch.await,
        Some(hang_up) => {
            let handle = tokio::spawn(dispatch); // determinism-ok: the caller, on this runtime's one thread
            match hang_up {
                HangUp::AfterTurns(turns) => {
                    for _ in 0..turns {
                        tokio::task::yield_now().await;
                    }
                }
                HangUp::AfterCommit => {
                    // Watch the store's commit count, not the journal: a
                    // read here would use up a read fault meant for the move.
                    let journal = journal_of(id);
                    let before = store.commits(&journal);
                    for _ in 0..2_000 {
                        if handle.is_finished() || store.commits(&journal) > before {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }
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

/// How many `Note`s the record's journal holds.
async fn noted(store: &SimEventStore, id: &str) -> i64 {
    let events = store
        .read_events(&journal_of(id), 0)
        .await
        .expect("the journal reads");
    events
        .iter()
        .filter(|event| event.event_type == "Note")
        .count() as i64
}

/// The `Hold`s the record's `Ledger` counted.
async fn ledger_count(state: &ServerState, id: &str) -> i64 {
    match state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Ledger", id)
        .await
    {
        Ok(ledger) => ledger.state.counters.get("holds").copied().unwrap_or(0) as i64,
        Err(_) => 0,
    }
}

/// The record's journal, replayed: (sequence, bumps, status).
async fn journal_state(store: &SimEventStore, id: &str) -> (u64, i64, String) {
    let (journal, bumps) = journal(store, id).await;
    (journal.sequence, bumps, journal.status)
}

/// The record's journal, replayed, and its bumps.
async fn journal(store: &SimEventStore, id: &str) -> (rows::Journal, i64) {
    let events = store
        .read_events(&journal_of(id), 0)
        .await
        .expect("the journal reads");
    let sequence = events.last().map(|event| event.sequence_nr).unwrap_or(0);
    let mut bumps = 0;
    let mut status = "Open".to_string();
    let mut busy_since = None;
    for event in &events {
        match event.event_type.as_str() {
            "Bump" => bumps += 1,
            "Hold" => {
                status = "Busy".to_string();
                busy_since = Some(event.sequence_nr);
            }
            "Release" | "Expire" => {
                status = "Open".to_string();
                busy_since = None;
            }
            _ => {}
        }
    }
    let journal = rows::Journal {
        sequence,
        status,
        busy_since,
    };
    (journal, bumps)
}

/// Note it when `id`'s rows do not say what its journal says.
async fn check_rows(run: &mut Run, store: &SimEventStore, id: &str) {
    let (journal, _) = journal(store, id).await;
    if let Err((invariant, detail)) = rows::rows_follow_journal(store, id, &journal) {
        run.note(invariant, detail);
    }
}

fn run_seed(seed: u64) -> Result<usize, Vec<String>> {
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
        noted: BTreeMap::new(),
    };
    let ended = runtime.block_on(async {
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
        // Per record: the `Hold`s that landed, and those of them whose
        // outcome could not be learned.
        let mut holds: BTreeMap<String, (i64, i64)> = BTreeMap::new();
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
            let hang_up = match rng.next_bound(10) {
                0 | 1 => Some(HangUp::AfterTurns(rng.next_bound(40))),
                2 | 3 => Some(HangUp::AfterCommit),
                _ => None,
            };
            fault.apply(&store, &journal);
            faulted += usize::from(fault != Fault::None);

            let commits_before = store.commits(&journal);
            let answer = move_once(&state, &store, &id, action, &mark, hang_up).await;
            // A hung-up move's actor keeps going; let its late writes land.
            tokio::time::sleep(Duration::from_secs(1)).await;
            settle().await;
            Fault::clear(&store, &journal);
            run.trace.push(format!(
                "{mark}: {action} {id}, {fault:?}, hang up {hang_up:?}: {answer:?}"
            ));

            let landings = landed(&store, &id, &mark).await;
            if action == "Hold" && landings == 1 {
                // Its caller was answered "outcome unknown", or would have
                // been: the journal read after its commit failed.
                let unknown = fault == Fault::LandThenFailUnreadable
                    && !matches!(answer, Answer::Success);
                let entry = holds.entry(id.clone()).or_default();
                entry.0 += 1;
                entry.1 += i64::from(unknown);
            }
            if landings > 1 {
                return Err(run.fail(
                    "a move lands at most once",
                    format!("{mark} landed {landings} times"),
                ));
            }
            let commits = store.commits(&journal) - commits_before;
            if commits > 1 {
                run.note(
                    "a move takes one commit",
                    format!("{mark} took {commits} commits"),
                );
            }
            check_rows(&mut run, &store, &id).await;
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

            {
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
            }
        }

        // A day later every timer has fired: no record waits in `Busy`.
        tokio::time::sleep(Duration::from_secs(86_400 + 60)).await;
        settle().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
        for id in (0..RECORDS).map(|i| format!("t-{i}")) {
            let (_, _, journal_status) = journal_state(&store, &id).await;
            // Each end check is noted and the others still run, so one broken
            // invariant does not hide another.
            if journal_status == "Busy" {
                run.note(
                    "every timer fires, whoever hung up",
                    format!("{id} is still Busy a day later"),
                );
            }
            check_rows(&mut run, &store, &id).await;
            let (landed_holds, unknown) = holds.get(&id).copied().unwrap_or_default();
            let notes = noted(&store, &id).await;
            if notes != landed_holds && notes != landed_holds - unknown {
                run.note(
                    "every landed hold has its scheduled note, whoever hung up",
                    format!("{id}: {landed_holds} holds landed ({unknown} unknown), {notes} notes"),
                );
            }
            let counted = ledger_count(&state, &id).await;
            if counted != landed_holds - unknown {
                run.note(
                    "every landed hold has its reaction, whoever hung up",
                    format!(
                        "{id}: {landed_holds} holds landed ({unknown} unknown), ledger counts {counted}"
                    ),
                );
            }
        }
        Ok(faulted)
    });
    let mut broken: Vec<String> = run.noted.into_values().collect();
    match ended {
        Ok(faulted) if broken.is_empty() => Ok(faulted),
        Ok(_) => Err(broken),
        Err(report) => {
            broken.insert(0, report);
            Err(broken)
        }
    }
}

#[test]
fn dst_a_moves_answer_is_what_its_commit_did() {
    let mut broken: BTreeMap<String, (Vec<u64>, String)> = BTreeMap::new();
    let mut faulted = 0;
    for seed in 0..SEEDS {
        match run_seed(seed) {
            Ok(moves) => faulted += moves,
            Err(reports) => {
                for report in reports {
                    let invariant = report.split(": ").nth(1).unwrap_or("unknown").to_string();
                    let entry = broken
                        .entry(invariant)
                        .or_insert_with(|| (Vec::new(), report));
                    entry.0.push(seed);
                }
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
