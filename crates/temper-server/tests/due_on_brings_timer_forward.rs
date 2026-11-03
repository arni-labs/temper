//! A `due_on` move brings its record's armed state timer forward.
//!
//! A webhook's hook records an outside event on a record whose state timer
//! the owner armed. Listed in the timer's `due_on`, that move makes the timer
//! due now: it fires at once, as the owner who armed it, with
//! `via == "timer"`, exactly as it would have at its deadline. The hook gains
//! only the moment, never the owner's identity: it cannot bring forward a
//! timer on another record, arm one that is not armed, or make a timer it
//! re-armed itself (a `reset_on` move) fire as anyone but the hook.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Due" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Change">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <Action Name="Watch" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Due.Change"/>
      </Action>
      <Action Name="WatchBriefly" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Due.Change"/>
      </Action>
      <Action Name="Reported" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Due.Change"/>
      </Action>
      <Action Name="Touch" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Due.Change"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Changes" EntityType="Temper.Due.Change"/>
        <EntitySet Name="Runs" EntityType="Temper.Due.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// The owner's `Watch` arms an hour-long timer in `Waiting` whose move,
/// `Check`, starts the record's `Run`. A hook's `Reported` brings it forward;
/// `Touch` re-arms it as whoever touched it. `WatchBriefly` arms a two-second
/// timer in `Brief`, to see who a re-armed timer fires as at its own deadline.
const CHANGE_IOA: &str = r#"
[automaton]
name = "Change"
states = ["Open", "Waiting", "Brief", "Checked"]
initial = "Open"
allow_indefinite_states = ["Open", "Checked"]

[[action]]
name = "Watch"
kind = "input"
from = ["Open"]
to = "Waiting"

[[action]]
name = "WatchBriefly"
kind = "input"
from = ["Open"]
to = "Brief"

[[action]]
name = "Reported"
kind = "input"
from = ["Open", "Waiting", "Brief"]

[[action]]
name = "Touch"
kind = "input"
from = ["Waiting", "Brief"]

[[state_timeout]]
state = "Waiting"
after_seconds = 3600
on_timeout = "Check"
reset_on = ["Touch"]
due_on = ["Reported"]

[[state_timeout]]
state = "Brief"
after_seconds = 2
on_timeout = "Check"
reset_on = ["Touch"]

[[action]]
name = "Check"
kind = "internal"
from = ["Waiting", "Brief"]
to = "Checked"

[[action.triggers]]
name = "starts"
kind = "entity"
target_entity = "Run"
target_action = "Start"
resolve_target = { kind = "same_id" }
"#;

const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Idle", "Started"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Started"]

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Started"
"#;

/// Everyone may do everything, except: the hook makes only its own moves by
/// request, and a run starts only from a timer the owner armed, never as the
/// hook. This is the shape of an app that keeps outside data from gaining its
/// owner's standing.
const POLICY: &str = r#"
permit(principal, action, resource);

forbid(principal, action, resource)
when { principal has agent_type && principal.agent_type == "katagami-hook" }
unless {
  action == Action::"Reported" || action == Action::"Touch" ||
  (context has via && context.via != "request")
};

forbid(principal, action == Action::"Start", resource)
unless {
  context has via && context.via == "timer" &&
  context has armedBy && context.armedBy == "owner-1"
};

forbid(principal, action == Action::"Start", resource)
when { principal has agent_type && principal.agent_type == "katagami-hook" };
"#;

/// The webhook's principal: an agent of type `katagami-hook` acting for its owner.
fn hook() -> SecurityContext {
    SecurityContext::from_verified_jwt(
        "hook-1",
        PrincipalKind::Agent,
        Some("katagami-hook"),
        Some("owner-1"),
        None,
        None,
    )
}

/// The records' owner.
fn owner() -> SecurityContext {
    SecurityContext::from_verified_jwt("owner-1", PrincipalKind::Customer, None, None, None, None)
}

fn server(name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let entities = [("Change", CHANGE_IOA), ("Run", RUN_IOA)];
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &entities,
    );
    for (entity_type, _) in &entities {
        registry.set_verification_status(
            &TenantId::default(),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-11-02T00:00:00Z".to_string(),
            }),
        );
    }
    let store = SimEventStore::no_faults(185);
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
}

async fn post(
    state: &ServerState,
    as_who: SecurityContext,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            as_who,
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A Change and its Run, both created by the owner.
async fn create(state: &ServerState, id: &str) {
    for set in ["Changes", "Runs"] {
        let (code, body) = post(
            state,
            owner(),
            &format!("/tdata/{set}"),
            json!({ "Id": id }),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{body:?}");
    }
}

async fn act(state: &ServerState, as_who: SecurityContext, id: &str, action: &str) {
    let path = format!("/tdata/Changes('{id}')/Temper.Due.{action}");
    let (code, body) = post(state, as_who, &path, json!({})).await;
    assert_eq!(code, StatusCode::OK, "{action} on {id}: {body:?}");
}

async fn status(state: &ServerState, entity_type: &str, id: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .expect("record should exist")
        .state
        .status
}

/// Wait until the record reaches `to`, or give up after `within`.
async fn reach(state: &ServerState, entity_type: &str, id: &str, to: &str, within: Duration) {
    let deadline = Instant::now() + within; // determinism-ok: real-time timer test
    while status(state, entity_type, id).await != to && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

/// Let anything already due land: reactions and timers due now.
async fn settle() {
    tokio::time::sleep(Duration::from_secs(2)).await; // determinism-ok: real-time test
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hooks_due_on_move_fires_the_owners_timer_now_as_the_owner() {
    let state = server("due-on-owner-timer");
    create(&state, "c1").await;
    act(&state, owner(), "c1", "Watch").await;
    assert_eq!(status(&state, "Change", "c1").await, "Waiting");

    // The owner's timer is due in an hour. The hook's report makes it due now.
    act(&state, hook(), "c1", "Reported").await;
    reach(&state, "Run", "c1", "Started", Duration::from_secs(10)).await;
    assert_eq!(
        status(&state, "Change", "c1").await,
        "Checked",
        "the owner's timer fired at once"
    );
    assert_eq!(
        status(&state, "Run", "c1").await,
        "Started",
        "its move's reaction ran as the owner, via the timer, armed by the owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_due_on_move_touches_only_its_own_records_armed_timer() {
    let state = server("due-on-own-record");
    for id in ["c2", "c3", "c4"] {
        create(&state, id).await;
    }
    act(&state, owner(), "c2", "Watch").await;
    act(&state, owner(), "c3", "Watch").await;

    // Another record's timer: c2's report leaves c3's timer at its deadline.
    act(&state, hook(), "c2", "Reported").await;
    reach(&state, "Run", "c2", "Started", Duration::from_secs(10)).await;
    assert_eq!(status(&state, "Run", "c2").await, "Started");
    settle().await;
    assert_eq!(status(&state, "Change", "c3").await, "Waiting");
    assert_eq!(status(&state, "Run", "c3").await, "Idle");

    // No armed timer: a report on a record in an untimed state arms nothing,
    // and a timer armed afterwards keeps its whole budget.
    act(&state, hook(), "c4", "Reported").await;
    settle().await;
    assert_eq!(status(&state, "Change", "c4").await, "Open");
    act(&state, owner(), "c4", "Watch").await;
    settle().await;
    assert_eq!(status(&state, "Change", "c4").await, "Waiting");
    assert_eq!(status(&state, "Run", "c4").await, "Idle");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_the_hook_reset_fires_as_the_hook_at_its_deadline() {
    let state = server("due-on-reset-deadline");
    create(&state, "c5").await;
    create(&state, "c6").await;

    // Unreset, the owner's brief timer starts the run at its deadline.
    act(&state, owner(), "c5", "WatchBriefly").await;
    reach(&state, "Run", "c5", "Started", Duration::from_secs(10)).await;
    assert_eq!(status(&state, "Run", "c5").await, "Started");

    // Reset by the hook, the same timer is the hook's: it fires at its new
    // deadline as the hook, and the run does not start.
    act(&state, owner(), "c6", "WatchBriefly").await;
    act(&state, hook(), "c6", "Touch").await;
    reach(&state, "Change", "c6", "Checked", Duration::from_secs(10)).await;
    assert_eq!(status(&state, "Change", "c6").await, "Checked");
    settle().await;
    assert_eq!(
        status(&state, "Run", "c6").await,
        "Idle",
        "a timer the hook re-armed fires with the hook's standing, not the owner's"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_the_hook_reset_and_then_brought_forward_fires_as_the_hook() {
    let state = server("due-on-reset-then-due");
    create(&state, "c7").await;
    act(&state, owner(), "c7", "Watch").await;

    // The hook's Touch re-arms the timer as the hook; its report then brings
    // that timer forward, and it fires now with the hook's standing.
    act(&state, hook(), "c7", "Touch").await;
    act(&state, hook(), "c7", "Reported").await;
    reach(&state, "Change", "c7", "Checked", Duration::from_secs(10)).await;
    assert_eq!(
        status(&state, "Change", "c7").await,
        "Checked",
        "the report brought the hook's timer forward"
    );
    settle().await;
    assert_eq!(
        status(&state, "Run", "c7").await,
        "Idle",
        "bringing a timer forward never gives it more standing than its armer"
    );
}
