//! A timer nothing saved names a setter for fires as the principal the
//! record's journal names: the one that last moved the record by request.
//!
//! A record that entered its timed state before timer rows were kept, or
//! whose row was lost, used to be armed at boot as the kernel's state-timer
//! service. A timer whose own move resets it then fired as the service for
//! good, and every move after it that needed a person was refused (Inspiration
//! Bank's Feed, 31 Oct 2026: its reactions withheld with principal
//! service:state-timer).
//!
//! Each test sets a record polling as someone, takes away or spoils what its
//! row says, restarts the server over the same store, and reads who the
//! reaction after the next polls ran as (`Merge` binds its caller).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::StateTimerStore;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

/// `Start` enters Checking, whose timer fires `Look` every second and resets
/// itself. Once `looks` reaches LOOKS, a reaction to `Look` makes `Merge`,
/// which binds whoever it runs as.
const WATCH_IOA: &str = r#"
[automaton]
name = "Watch"
states = ["Idle", "Checking", "Merged"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Merged"]
strict_action_params = true

[[state]]
name = "looks"
type = "counter"
initial = 0

[[state]]
name = "merged_by"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Checking"
params = []

[[state_timeout]]
state = "Checking"
after_seconds = 1
on_timeout = "Look"
reset_on = ["Look"]

[[action]]
name = "Look"
kind = "internal"
from = ["Checking"]
params = []
effect = ["looks += 1"]

[[action.triggers]]
name = "merge_when_ready"
kind = "entity"
target_entity = "Watch"
target_action = "Merge"
guard = "looks >= LOOKS"
resolve_target = { kind = "same_id" }

[[action]]
name = "Merge"
kind = "input"
from = ["Checking"]
to = "Merged"
params = [{ name = "merged_by", type = "string", source = "authenticated_subject" }]
"#;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Watch" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Watch">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="looks" Type="Edm.Int64"/>
        <Property Name="merged_by" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Watches" EntityType="Temper.Watch.Watch"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const TENANT: &str = "default";

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

/// A started server over `store`, merging after `looks` looks.
async fn server(name: &str, store: &SimEventStore, looks: u32) -> ServerState {
    let ioa = WATCH_IOA.replace("LOOKS", &looks.to_string());
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &[("Watch", ioa.as_str())],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal(),
    );
    state.state_timer_rows = Some(Arc::new(CommittedRows(store.clone())));
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
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

/// An agent acting for `person` with `role`.
fn agent_for(person: &str, role: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("claude-code"),
        Some(person),
        Some(role),
        None,
    ))
}

/// What becomes of the record's timer row while the server is down.
#[derive(Clone, Copy, Debug)]
enum Spoil {
    /// The row is gone, as for a record that entered its state before rows
    /// were kept.
    Lost,
    /// The row says the timer was armed for no one, as a boot that found no
    /// row left it.
    Service,
}

/// The row's `armed_by` for a timer armed as the state-timer service.
fn service_armed_by() -> String {
    let service = AgentContext::for_service("state-timer");
    json!({
        "security_ctx": service.security_ctx,
        "agent_id": service.agent_id,
        "agent_type": service.agent_type,
    })
    .to_string()
}

async fn watch(state: &ServerState, id: &str) -> (String, serde_json::Map<String, Value>) {
    let record = state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Watch", id)
        .await
        .expect("the watch exists")
        .state;
    let fields = record.fields.as_object().cloned().unwrap_or_default();
    (record.status, fields)
}

/// Wait until watch `id` is `status`, or fail saying where it stood.
async fn reaches(
    state: &ServerState,
    id: &str,
    status: &str,
    within: Duration,
) -> Result<serde_json::Map<String, Value>, String> {
    let deadline = Instant::now() + within; // determinism-ok: real-time test
    loop {
        let (now, fields) = watch(state, id).await;
        if now == status {
            return Ok(fields);
        }
        if Instant::now() >= deadline {
            // determinism-ok: real-time test
            return Err(format!("watch {id} stayed {now} with {fields:?}"));
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

/// Start watch `id` polling as `by`, spoil its row as `spoil` while the
/// server is down, and return the server started after, which merges at the
/// next look.
async fn poll_then_restart(id: &str, by: &AgentContext, spoil: Spoil) -> ServerState {
    let store = SimEventStore::no_faults(7);
    let (first_store, first_id, by) = (store.clone(), id.to_string(), by.clone());
    std::thread::spawn(move || {
        // Its own runtime: dropping it stops the server as a process exit
        // would, timers and all.
        let first = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("runtime");
        first.block_on(async {
            let state = server("watch-before", &first_store, 1000).await;
            let tenant = TenantId::new(TENANT);
            state
                .get_or_create_tenant_entity(&tenant, "Watch", &first_id, json!({}))
                .await
                .expect("the watch is created");
            let started = state
                .dispatch_tenant_action(&tenant, "Watch", &first_id, "Start", json!({}), &by)
                .await
                .expect("started");
            assert!(started.success, "{:?}", started.error);
            // The row the process leaves was armed by a look, not by Start.
            reaches_looks(&state, &first_id, 2).await;
        });
        drop(first);
    })
    .join()
    .expect("the first server ran");

    let rows = store.list_state_timers(TENANT);
    let row = rows
        .iter()
        .find(|row| row.entity_id == id)
        .expect("the poll left a timer row")
        .clone();
    match spoil {
        Spoil::Lost => store.delete_state_timer(TENANT, "Watch", id, &row.arm_id),
        Spoil::Service => store.upsert_state_timer(&StateTimerRecord {
            armed_by: service_armed_by(),
            ..row
        }),
    }
    server(&format!("{id}-after"), &store, 1).await
}

async fn reaches_looks(state: &ServerState, id: &str, looks: i64) {
    let deadline = Instant::now() + Duration::from_secs(20); // determinism-ok: real-time test
    loop {
        let (_, fields) = watch(state, id).await;
        if fields.get("looks").and_then(Value::as_i64).unwrap_or(0) >= looks {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "watch {id} never looked {looks} times"
        ); // determinism-ok: real-time test
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

async fn merged_by(id: &str, by: &AgentContext, spoil: Spoil) -> String {
    let state = poll_then_restart(id, by, spoil).await;
    let fields = reaches(&state, id, "Merged", Duration::from_secs(20))
        .await
        .unwrap_or_else(|error| panic!("{spoil:?}: the reaction after a look never ran: {error}"));
    fields
        .get("merged_by")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_rows_poll_runs_as_the_person_who_started_it() {
    assert_eq!(
        merged_by("w-lost", &person("person-1"), Spoil::Lost).await,
        "person-1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_service_armed_poll_runs_as_the_person_who_started_it() {
    assert_eq!(
        merged_by("w-service", &person("person-2"), Spoil::Service).await,
        "person-2"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_rows_poll_runs_as_the_agent_that_started_it() {
    assert_eq!(
        merged_by(
            "w-agent",
            &agent_for("person-3", "contributor"),
            Spoil::Lost
        )
        .await,
        "person-3"
    );
}

/// A record nobody moved by request keeps its timer for no one: the journal
/// names no one, so its reaction, which needs a caller, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn a_poll_nobody_requested_still_runs_for_no_one() {
    let state = poll_then_restart("w-system", &AgentContext::system(), Spoil::Lost).await;
    assert!(
        reaches(&state, "w-system", "Merged", Duration::from_secs(4))
            .await
            .is_err(),
        "a timer whose journal names no one must not act for anyone"
    );
}
