//! A module that answers `retry` is called again later, until it answers or
//! its trigger's deadline passes.
//!
//! A `Lookup` record's `Ask` starts the `lookup` module in the background;
//! its trigger answers `Finish` on success and `GiveUp` on failure, with a
//! 30-second deadline (`timeout_secs`). A module that cannot answer yet (a
//! service it calls is down, a lease is held) answers
//! `{"retry": true, "success": false, "error": <why>}`:
//!
//! - nothing moves, and the module is called again with the same trigger and
//!   params, first a second later and then twice as long after each try; the
//!   context it reads says which try it is (`attempt`, from 1);
//! - once the deadline, counted from the first try, passes, the trigger's
//!   `on_failure` is sent, once;
//! - a record that has moved on since the move that started the call is not
//!   called for again: the call is dropped, and nothing is left behind to
//!   answer it later.
//!
//! The real WASM engine runs the module, in a single-threaded, paused-clock
//! Tokio runtime whose clock the simulation reads.
//!
//! Engine-boundary exception: the real Wasmtime worker runs outside the
//! simulator. The bounded wall-clock waits below give it CPU while keeping
//! Tokio's virtual clock paused; every wait between tries uses explicit
//! virtual time. These waits prove integration, not DST replay.

#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use std::sync::Arc;
use std::time::Duration;

use process_clock::ProcessClock;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const TENANT: &str = "retry-answers";
const ID: &str = "lookup-1";
const JOURNAL: &str = "retry-answers:Lookup:lookup-1";

/// The call's deadline, as its trigger declares it.
const DEADLINE_SECS: u64 = 30;

const LOOKUP: &str = r#"
[automaton]
name = "Lookup"
states = ["Idle", "Asked", "Done", "GaveUp"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Asked", "Done", "GaveUp"]

[[state]]
name = "notes"
type = "counter"
initial = 0

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "lookup"
kind = "wasm"
module = "lookup"
on_success = "Finish"
on_failure = "GiveUp"

[action.triggers.config]
timeout_secs = "30"

[[action]]
name = "Note"
kind = "input"
from = ["Asked"]
effect = ["notes += 1"]

[[action]]
name = "Finish"
kind = "internal"
from = ["Asked"]
to = "Done"
params = [{ name = "score", type = "int" }]

[[action]]
name = "GiveUp"
kind = "internal"
from = ["Asked"]
to = "GaveUp"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Lookups" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Lookup"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<EntityContainer Name="Container"><EntitySet Name="Lookups" EntityType="Lookups.Lookup"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

/// The answer a module gives when it cannot answer yet.
fn retry() -> Value {
    json!({ "retry": true, "success": false, "error": "the service is down" })
}

/// WAT string data for `text`.
fn wat_bytes(text: &str) -> String {
    text.bytes().map(|byte| format!("\\{byte:02x}")).collect()
}

/// A module that answers `then` when the context it is called with contains
/// `needle`, and `otherwise` when it does not.
fn answering_when(needle: &str, then: &Value, otherwise: &Value) -> String {
    let (then, otherwise) = (then.to_string(), otherwise.to_string());
    assert!(needle.len() < 256 && then.len() < 768 && otherwise.len() < 1024);
    format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{needle_data}")
        (data (i32.const 256) "{then_data}")
        (data (i32.const 1024) "{otherwise_data}")
        (func $has_needle (param $ctx i32) (param $len i32) (result i32)
            (local $at i32) (local $i i32)
            (block $none
                (loop $start
                    (br_if $none (i32.gt_s (i32.add (local.get $at) (i32.const {needle_len})) (local.get $len)))
                    (local.set $i (i32.const 0))
                    (block $differs
                        (loop $byte
                            (if (i32.eq (local.get $i) (i32.const {needle_len})) (then (return (i32.const 1))))
                            (br_if $differs (i32.ne
                                (i32.load8_u (i32.add (local.get $ctx) (i32.add (local.get $at) (local.get $i))))
                                (i32.load8_u (local.get $i))))
                            (local.set $i (i32.add (local.get $i) (i32.const 1)))
                            (br $byte)))
                    (local.set $at (i32.add (local.get $at) (i32.const 1)))
                    (br $start)))
            (i32.const 0))
        (func (export "run") (param $ctx i32) (param $len i32) (result i32)
            (if (call $has_needle (local.get $ctx) (local.get $len))
                (then (call $result (i32.const 256) (i32.const {then_len})))
                (else (call $result (i32.const 1024) (i32.const {otherwise_len}))))
            i32.const 0))"#,
        needle_data = wat_bytes(needle),
        then_data = wat_bytes(&then),
        otherwise_data = wat_bytes(&otherwise),
        needle_len = needle.len(),
        then_len = then.len(),
        otherwise_len = otherwise.len(),
    )
}

/// One server process over `store`, its `lookup` module compiled from `wat`.
struct Process {
    runtime: tokio::runtime::Runtime,
    state: ServerState,
}

impl Process {
    fn start(store: &SimEventStore, clock: &ProcessClock, wat: &str) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let state = runtime.block_on(async {
            clock.begin();
            let mut registry = SpecRegistry::new();
            let csdl = parse_csdl(CSDL).unwrap();
            registry.register_tenant(TENANT, csdl, CSDL.to_string(), &[("Lookup", LOOKUP)]);
            let mut state = ServerState::from_registry(ActorSystem::new("retry-answers"), registry);
            state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
            let hash = state
                .wasm_engine
                .compile_and_cache(wat.as_bytes())
                .expect("the module compiles");
            state.register_wasm_module(&TenantId::new(TENANT), "lookup", &hash);
            let walk = state.start_runtime().await.expect("the first start");
            walk.await.unwrap();
            state
        });
        Self { runtime, state }
    }

    /// Exit, carrying the clock it ran to the next process.
    fn stop(self, clock: &ProcessClock) {
        self.runtime.block_on(async { clock.end(Duration::ZERO) });
    }
}

/// Create the record and send `Ask`, as the system.
async fn ask(state: &ServerState) {
    let tenant = TenantId::new(TENANT);
    state
        .get_or_create_tenant_entity(&tenant, "Lookup", ID, json!({}))
        .await
        .expect("the record is created");
    let asked = state
        .dispatch_tenant_action(
            &tenant,
            "Lookup",
            ID,
            "Ask",
            json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("Ask dispatches");
    assert!(asked.success, "Ask is taken: {:?}", asked.error);
}

/// Let the module's blocking thread and the tasks it wakes run for a while
/// of real time, without letting the paused clock move.
async fn settle() {
    for _ in 0..300 {
        std::thread::sleep(Duration::from_millis(1)); // determinism-ok: real WASM blocking worker needs CPU; virtual time must stay paused
        tokio::task::yield_now().await;
    }
}

/// Move the paused clock on by `secs`, a second at a time, letting each
/// second's work run.
async fn pass(secs: u64) {
    for _ in 0..secs {
        tokio::time::sleep(Duration::from_secs(1)).await;
        settle().await;
    }
}

/// The actions of the record's journal, in order.
async fn journal(store: &SimEventStore) -> Vec<String> {
    store
        .read_events(JOURNAL, 0)
        .await
        .expect("the journal reads")
        .into_iter()
        .map(|event| event.event_type)
        .collect()
}

/// The record's status as the process reads it.
async fn status(state: &ServerState) -> String {
    state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Lookup", ID)
        .await
        .expect("the record reads")
        .state
        .status
}

#[test]
fn a_module_that_answers_retry_is_called_again_until_it_answers() {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(11)));
    let store = SimEventStore::no_faults(11);
    // The module answers only on its third try, which its context names.
    let wat = answering_when(
        r#""attempt":3"#,
        &json!({ "params": { "score": 70 } }),
        &retry(),
    );
    let process = Process::start(&store, &clock, &wat);
    process.runtime.block_on(async {
        ask(&process.state).await;
        settle().await;
        assert_eq!(
            journal(&store).await,
            ["Created", "Ask"],
            "a retry moves nothing"
        );
        assert_eq!(status(&process.state).await, "Asked");
        // Tries at 0 s, 1 s and 3 s: the third answers, well before the
        // deadline. The clock moves a second at a time until it has.
        let mut waited = 0;
        while waited < DEADLINE_SECS - 5 && status(&process.state).await == "Asked" {
            pass(1).await;
            waited += 1;
        }
        let events = journal(&store).await;
        assert_eq!(
            events,
            ["Created", "Ask", "Finish"],
            "answered once, never given up, after {waited} s"
        );
        assert_eq!(status(&process.state).await, "Done");
        let rows = store.list_scheduled_actions(TENANT).await.unwrap();
        assert!(rows.is_empty(), "the answer ends the call: {rows:?}");
        // Nothing is left to give up on it later.
        pass(2 * DEADLINE_SECS).await;
        assert_eq!(journal(&store).await, ["Created", "Ask", "Finish"]);
    });
    process.stop(&clock);
}

#[test]
fn a_module_that_keeps_answering_retry_is_given_up_on_once_at_its_deadline() {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(12)));
    let store = SimEventStore::no_faults(12);
    let wat = answering_when("never in a context", &json!({}), &retry());
    let process = Process::start(&store, &clock, &wat);
    process.runtime.block_on(async {
        ask(&process.state).await;
        settle().await;
        pass(DEADLINE_SECS - 2).await;
        assert_eq!(
            journal(&store).await,
            ["Created", "Ask"],
            "the call is still owed before its deadline"
        );
        assert_eq!(status(&process.state).await, "Asked");
        pass(4).await;
        let events = journal(&store).await;
        assert_eq!(
            events,
            ["Created", "Ask", "GiveUp"],
            "given up on once, at its deadline"
        );
        assert_eq!(status(&process.state).await, "GaveUp");
        let rows = store.list_scheduled_actions(TENANT).await.unwrap();
        assert!(rows.is_empty(), "the on_failure ends the call: {rows:?}");
        pass(2 * DEADLINE_SECS).await;
        assert_eq!(journal(&store).await, ["Created", "Ask", "GiveUp"]);
    });
    process.stop(&clock);
}

#[test]
fn a_retry_is_dropped_once_the_record_moves_on() {
    let clock = Arc::new(ProcessClock::new());
    let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(13)));
    let store = SimEventStore::no_faults(13);
    let wat = answering_when("never in a context", &json!({}), &retry());
    let process = Process::start(&store, &clock, &wat);
    process.runtime.block_on(async {
        ask(&process.state).await;
        settle().await;
        // Another move on the record before the call's next try.
        let noted = process
            .state
            .dispatch_tenant_action(
                &TenantId::new(TENANT),
                "Lookup",
                ID,
                "Note",
                json!({}),
                &AgentContext::system(),
            )
            .await
            .expect("Note dispatches");
        assert!(noted.success, "Note is taken: {:?}", noted.error);
        pass(2 * DEADLINE_SECS).await;
        let events = journal(&store).await;
        assert_eq!(
            events,
            ["Created", "Ask", "Note"],
            "a call the record moved on from is neither tried again nor given up on"
        );
        let rows = store.list_scheduled_actions(TENANT).await.unwrap();
        assert!(rows.is_empty(), "the dropped call leaves nothing: {rows:?}");
    });
    process.stop(&clock);
}
