//! A timer fires as whoever its record's moves name, and a timer nothing names
//! a caller for stops and says so.
//!
//! A reaction carries its person in memory, but a record made and moved only
//! by reactions kept no one in its journal, so once its timer row was gone
//! nothing could name who its timer fires as: it waited forever, or fired as
//! the state-timer service, whose move binding its caller was refused on every
//! fire, unseen (the Arni Labs line's Rollout, 31 Oct 2026).

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

/// Answers `{"seen": ...}`.
const ECHO_WASM: &[u8] =
    include_bytes!("../../../crates/temper-wasm/tests/fixtures/trigger_principal_echo.wasm");

/// `Merge` (someone's request) sets off a reaction, `RollOut`, whose effect
/// spawns the Shipment. The Shipment is made and moved only by that chain.
const LINE_IOA: &str = r#"
[automaton]
name = "Line"
states = ["Open", "Merging", "Shipping"]
initial = "Open"
allow_indefinite_states = ["Open", "Merging", "Shipping"]
strict_action_params = true

[[state]]
name = "rollout"
type = "string"
initial = ""

[[action]]
name = "Tidy"
kind = "input"
from = ["Open"]
params = []

[[action]]
name = "Merge"
kind = "input"
from = ["Open"]
to = "Merging"
params = []

[[action.triggers]]
name = "merged"
kind = "entity"
target_entity = "Line"
target_action = "RollOut"
resolve_target = { kind = "same_id" }

[[action]]
name = "RollOut"
kind = "input"
from = ["Merging"]
to = "Shipping"
params = []
effect = ["spawn('Shipment', 'Start', rollout)"]
"#;

/// `Start` enters `Staging`, whose timer fires `Diagnose`, which binds whoever
/// it runs as.
const SHIPMENT_IOA: &str = r#"
[automaton]
name = "Shipment"
states = ["Idle", "Staging", "Diagnosed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Diagnosed"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Staging"
params = ["parent_type", "parent_id", "line_id"]

[[action]]
name = "Nudge"
kind = "input"
from = ["Staging"]
params = []

[[state_timeout]]
state = "Staging"
after_seconds = AFTER
on_timeout = "Diagnose"

[[action]]
name = "Diagnose"
kind = "internal"
from = ["Staging"]
to = "Diagnosed"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// A timed action that binds its caller. `Nudge` is a move in the timed state
/// that changes nothing else.
const CHECK_IOA: &str = r#"
[automaton]
name = "Check"
states = ["Idle", "Checking", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Checking"
params = []

[[action]]
name = "Nudge"
kind = "input"
from = ["Checking"]
params = []

[[state_timeout]]
state = "Checking"
after_seconds = 1
on_timeout = "Finish"

[[action]]
name = "Finish"
kind = "internal"
from = ["Checking"]
to = "Done"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// The line's Rollout: a self-resetting timed `Poll` binds no one and lands,
/// and a reaction to it, `Diagnose`, binds its caller.
const POLLER_IOA: &str = r#"
[automaton]
name = "Poller"
states = ["Idle", "Polling", "Stuck"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Stuck"]
strict_action_params = true

[[state]]
name = "polls"
type = "counter"
initial = 0

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Polling"
params = []

[[action]]
name = "Nudge"
kind = "input"
from = ["Polling"]
params = []

[[state_timeout]]
state = "Polling"
after_seconds = 1
on_timeout = "Poll"
reset_on = ["Poll"]

[[action]]
name = "Poll"
kind = "internal"
from = ["Polling"]
params = []
effect = ["polls += 1"]

[[action.triggers]]
name = "diagnose"
kind = "entity"
target_entity = "Poller"
target_action = "Diagnose"
resolve_target = { kind = "same_id" }

[[action]]
name = "Diagnose"
kind = "input"
from = ["Polling"]
to = "Stuck"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

/// The same, with the refusal after an integration's answer, so well after the
/// timer's fire returned and its next arm was set.
const ASKER_IOA: &str = r#"
[automaton]
name = "Asker"
states = ["Idle", "Polling", "Failed", "Stuck"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Failed", "Stuck"]
strict_action_params = true

[[state]]
name = "polls"
type = "counter"
initial = 0

[[state]]
name = "seen"
type = "string"
initial = ""

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Polling"
params = []

[[state_timeout]]
state = "Polling"
after_seconds = 1
on_timeout = "Poll"
reset_on = ["Poll"]

[[action]]
name = "Poll"
kind = "internal"
from = ["Polling"]
params = []
effect = ["polls += 1"]

[[action.triggers]]
name = "ask"
kind = "wasm"
module = "trigger_principal_echo"
on_success = "Saw"
on_failure = "Fail"

[[action]]
name = "Saw"
kind = "input"
from = ["Polling"]
params = ["seen"]

[[action.triggers]]
name = "diagnose"
kind = "entity"
target_entity = "Asker"
target_action = "Diagnose"
resolve_target = { kind = "same_id" }

[[action]]
name = "Fail"
kind = "input"
from = ["Polling"]
to = "Failed"
params = []

[[action]]
name = "Diagnose"
kind = "input"
from = ["Polling"]
to = "Stuck"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]
"#;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Needs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Line">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="rollout" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Shipment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="parent_type" Type="Edm.String"/>
        <Property Name="parent_id" Type="Edm.String"/>
        <Property Name="line_id" Type="Edm.String"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Check">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Poller">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="polls" Type="Edm.Int64"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Asker">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="polls" Type="Edm.Int64"/>
        <Property Name="seen" Type="Edm.String"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Lines" EntityType="Temper.Needs.Line"/>
        <EntitySet Name="Shipments" EntityType="Temper.Needs.Shipment"/>
        <EntitySet Name="Checks" EntityType="Temper.Needs.Check"/>
        <EntitySet Name="Pollers" EntityType="Temper.Needs.Poller"/>
        <EntitySet Name="Askers" EntityType="Temper.Needs.Asker"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const WITHIN: Duration = Duration::from_secs(20);

/// The log line the kernel leaves each time it stops a timer.
const STOPPED: &str = "state timer stopped: nothing names its caller";

#[derive(Clone, Default)]
struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLog {
    /// Capture this test process's log. Each test is its own process in CI.
    fn install() -> Self {
        let logs = Self::default();
        let writer = logs.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .json()
                .without_time()
                .with_writer(move || writer.clone())
                .finish(),
        )
        .expect("the log is captured once per process");
        logs
    }

    /// The times a timer of record `id` was stopped.
    fn stops(&self, id: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|event| {
                event["fields"]["message"] == STOPPED && event["fields"]["entity_id"] == id
            })
            .count()
    }
}

/// Timer rows committed with the journal's events, as on Turso.
struct CommittedRows(SimEventStore);

#[async_trait::async_trait]
impl StateTimerStore for CommittedRows {
    async fn upsert_state_timer(&self, row: &StateTimerRecord) -> Result<(), PersistenceError> {
        self.0.upsert_state_timer(row);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// A started server over `store`, with Shipment's timer at `after` seconds.
async fn server(name: &str, store: &SimEventStore, after: u32) -> ServerState {
    let shipment = SHIPMENT_IOA.replace("AFTER", &after.to_string());
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &[
            ("Line", LINE_IOA),
            ("Shipment", shipment.as_str()),
            ("Check", CHECK_IOA),
            ("Poller", POLLER_IOA),
            ("Asker", ASKER_IOA),
        ],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal(),
    );
    state.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("the module compiles");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock") // ci-ok: infallible lock
        .register(&TenantId::default(), "trigger_principal_echo", &hash);
    state.start_runtime().await;
    state
}

fn as_who(security_ctx: SecurityContext) -> AgentContext {
    AgentContext {
        security_ctx: Some(security_ctx),
        ..AgentContext::default()
    }
}

fn person(id: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        id,
        PrincipalKind::Customer,
        None,
        None,
        None,
        None,
    ))
}

fn agent_for(person: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude-code"),
        Some(person),
        Some("contributor"),
        None,
    ))
}

/// Make `action` on `entity_type` `id` as `by`, creating the record first.
async fn act(state: &ServerState, entity_type: &str, id: &str, action: &str, by: &AgentContext) {
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, entity_type, id, json!({}))
        .await
        .expect("the record is created");
    let response = state
        .dispatch_tenant_action(&tenant, entity_type, id, action, json!({}), by)
        .await
        .expect("the move is dispatched");
    assert!(
        response.success,
        "{action} on {entity_type} {id}: {:?}",
        response.error
    );
}

/// The record as an ordinary read returns it.
async fn read(state: &ServerState, set: &str, id: &str) -> Value {
    let mut request = Request::builder()
        .method("GET")
        .uri(format!("/tdata/{set}('{id}')"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("reader", "person", None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

/// Wait until `holds` holds of the record, or fail saying what it read.
async fn until(
    state: &ServerState,
    set: &str,
    id: &str,
    what: &str,
    holds: fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + WITHIN; // determinism-ok: real-time test
    loop {
        let record = read(state, set, id).await;
        if holds(&record) {
            return record;
        }
        assert!(
            Instant::now() < deadline, // determinism-ok: real-time test
            "{set} {id} never {what}: {record}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

fn is_stopped(record: &Value) -> bool {
    record["timer_needs_person"].is_string()
}

/// Run `first` on a server of its own, stopped afterward as a process exit
/// would stop it, timers and all, and return what it returned.
fn on_a_server_that_stops<T, F, Fut>(store: &SimEventStore, after: u32, first: F) -> T
where
    T: Send + 'static,
    F: FnOnce(ServerState) -> Fut + Send + 'static,
    Fut: Future<Output = T>,
{
    let store = store.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let answer = runtime.block_on(async move {
            let state = server("first", &store, after).await;
            first(state).await
        });
        drop(runtime);
        answer
    })
    .join()
    .expect("the first server ran")
}

/// A record made and moved only by a chain, and who the chain says it ran as.
struct Shipped {
    /// Who the shipment's newest move says it ran as, before the restart.
    moved_by: Value,
    /// Who its timer fired as after losing its row.
    fired_by: Value,
}

/// `by`'s request sets off a reaction whose effect makes the Shipment, its timer
/// row is lost while the server is down, and the server restarts. `owner`, when
/// given, moved the Line first.
async fn ship_and_lose_the_row(owner: Option<AgentContext>, by: AgentContext) -> Shipped {
    let store = SimEventStore::no_faults(7);
    let (shipment, moved_by) = on_a_server_that_stops(&store, 3600, |state| async move {
        if let Some(owner) = owner {
            act(&state, "Line", "line", "Tidy", &owner).await;
        }
        act(&state, "Line", "line", "Merge", &by).await;
        let line = until(&state, "Lines", "line", "shipped", |line| {
            line["fields"]["rollout"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        })
        .await;
        let shipment = line["fields"]["rollout"].as_str().unwrap().to_string();
        let record = until(&state, "Shipments", &shipment, "started", |record| {
            record["status"] == "Staging"
        })
        .await;
        (shipment, record["last_event_by"].clone())
    });
    let rows = store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == shipment)
        .expect("the shipment's timer left a row");
    store.delete_state_timer("default", "Shipment", &shipment, &row.arm_id);

    let state = server("after", &store, 1).await;
    let record = until(&state, "Shipments", &shipment, "fired", |record| {
        record["status"] == "Diagnosed"
    })
    .await;
    Shipped {
        moved_by,
        fired_by: record["fields"]["fired_by"].clone(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_a_person_started_keeps_a_lost_rows_timer_as_that_person() {
    let shipped = ship_and_lose_the_row(None, person("ann")).await;
    assert_eq!(shipped.moved_by, "ann");
    assert_eq!(shipped.fired_by, "ann");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chain_an_agent_started_keeps_a_lost_rows_timer_as_that_agent() {
    let shipped = ship_and_lose_the_row(None, agent_for("bea")).await;
    assert_eq!(shipped.moved_by, "agent-1");
    assert_eq!(
        shipped.fired_by, "bea",
        "the action binds whom the agent acts for"
    );
}

/// A guest's chain on a record its owner moved first names the guest, never
/// the owner.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_a_non_owner_started_names_them_not_the_owner() {
    let shipped = ship_and_lose_the_row(Some(person("owner")), person("guest")).await;
    assert_eq!(shipped.moved_by, "guest");
    assert_eq!(shipped.fired_by, "guest");
}

/// A timer that never fires, since its action binds a caller and nothing names
/// one when it is found with no row. Its record says so, once, and a person's
/// move runs it.
#[tokio::test(flavor = "multi_thread")]
async fn a_timer_waiting_for_a_caller_says_so() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    on_a_server_that_stops(&store, 3600, |state| async move {
        let tenant = TenantId::default();
        state
            .get_or_create_tenant_entity(&tenant, "Shipment", "s-1", json!({}))
            .await
            .expect("the record is created");
        let started = state
            .dispatch_tenant_action(
                &tenant,
                "Shipment",
                "s-1",
                "Start",
                json!({"parent_type": "", "parent_id": "", "line_id": ""}),
                &AgentContext::system(),
            )
            .await
            .expect("the move is dispatched");
        assert!(started.success, "{:?}", started.error);
    });
    let rows = store.list_state_timers("default");
    let row = rows
        .iter()
        .find(|row| row.entity_id == "s-1")
        .expect("the timer left a row");
    store.delete_state_timer("default", "Shipment", "s-1", &row.arm_id);

    let state = server("after", &store, 1).await;
    let record = until(
        &state,
        "Shipments",
        "s-1",
        "said it needs a person",
        is_stopped,
    )
    .await;
    assert_eq!(record["timer_needs_person"], "Diagnose", "{record}");
    assert_eq!(record["status"], "Staging");
    drop(state);
    let state = server("again", &store, 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await; // determinism-ok: real-time test
    assert_eq!(
        logs.stops("s-1"),
        1,
        "a record already saying so says it once"
    );

    act(&state, "Shipment", "s-1", "Nudge", &person("gil")).await;
    let record = until(&state, "Shipments", "s-1", "fired", |record| {
        record["status"] == "Diagnosed"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "gil");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// A chain no one started: the timed action that binds its caller fires once
/// and is refused. It does not fire again, even after a restart. The record
/// says so until a person's move, whose caller its timer then fires as.
#[tokio::test(flavor = "multi_thread")]
async fn a_timed_action_no_one_can_make_stops_until_a_person_moves() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    on_a_server_that_stops(&store, 1, |state| async move {
        act(&state, "Check", "c-1", "Begin", &AgentContext::system()).await;
        until(
            &state,
            "Checks",
            "c-1",
            "said it needs a person",
            is_stopped,
        )
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    });
    assert_eq!(logs.stops("c-1"), 1, "it fired once and was refused once");
    assert!(
        store
            .list_state_timers("default")
            .iter()
            .all(|row| row.entity_id != "c-1"),
        "the stopped timer keeps no row"
    );

    let state = server("after", &store, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let record = read(&state, "Checks", "c-1").await;
    assert_eq!(record["timer_needs_person"], "Finish", "{record}");
    assert_eq!(record["status"], "Checking");
    assert_eq!(logs.stops("c-1"), 1, "a restart does not fire it again");

    act(&state, "Check", "c-1", "Nudge", &person("cal")).await;
    let record = until(&state, "Checks", "c-1", "finished", |record| {
        record["status"] == "Done"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "cal");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// The line's Rollout: the timed move lands and a reaction to it is refused.
/// On a timer that keeps resetting itself, that is a refusal on every fire.
#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_no_one_can_make_stops_a_repeating_timer() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    on_a_server_that_stops(&store, 1, |state| async move {
        act(&state, "Poller", "p-1", "Begin", &AgentContext::system()).await;
        until(
            &state,
            "Pollers",
            "p-1",
            "said it needs a person",
            is_stopped,
        )
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    });
    let state = server("after", &store, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let record = read(&state, "Pollers", "p-1").await;
    assert_eq!(
        record["fields"]["polls"], 1,
        "one fire, restart or not: {record}"
    );
    assert_eq!(record["timer_needs_person"], "Diagnose", "{record}");
    assert_eq!(logs.stops("p-1"), 1);

    act(&state, "Poller", "p-1", "Nudge", &person("dee")).await;
    let record = until(&state, "Pollers", "p-1", "got stuck", |record| {
        record["status"] == "Stuck"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "dee");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// The same when the refusal comes from an integration's answer: the timer's
/// fire has returned and its next arm is set, and it is still the one stopped.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_after_an_integrations_answer_stops_the_timer_that_asked() {
    let logs = CapturedLog::install();
    let store = SimEventStore::no_faults(7);
    let state = server("asker", &store, 1).await;
    act(&state, "Asker", "a-1", "Begin", &AgentContext::system()).await;
    until(
        &state,
        "Askers",
        "a-1",
        "said it needs a person",
        is_stopped,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let record = read(&state, "Askers", "a-1").await;
    assert_eq!(record["timer_needs_person"], "Diagnose", "{record}");
    assert!(logs.stops("a-1") >= 1);
    tokio::time::sleep(Duration::from_secs(3)).await; // determinism-ok: real-time test
    let later = read(&state, "Askers", "a-1").await;
    assert_eq!(
        later["fields"]["polls"], record["fields"]["polls"],
        "it stopped polling: {later}"
    );
}

/// A person's move finds a timer armed for no one and keeps it, as it keeps
/// any live timer. Its refusal then finds the person in the journal: it runs
/// as them, and the record never says it needs anyone.
#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_after_a_persons_move_runs_as_that_person() {
    let store = SimEventStore::no_faults(7);
    let state = server("kept", &store, 1).await;
    act(&state, "Check", "c-2", "Begin", &AgentContext::system()).await;
    act(&state, "Check", "c-2", "Nudge", &person("fay")).await;
    let record = until(&state, "Checks", "c-2", "finished", |record| {
        record["status"] == "Done"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "fay");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}

/// A person's chain never stops: this is the same Poller, started by a person.
#[tokio::test(flavor = "multi_thread")]
async fn a_chain_a_person_started_is_not_stopped() {
    let store = SimEventStore::no_faults(7);
    let state = server("named", &store, 1).await;
    act(&state, "Poller", "p-2", "Begin", &person("eve")).await;
    let record = until(&state, "Pollers", "p-2", "got stuck", |record| {
        record["status"] == "Stuck"
    })
    .await;
    assert_eq!(record["fields"]["fired_by"], "eve");
    assert_eq!(record["timer_needs_person"], Value::Null, "{record}");
}
