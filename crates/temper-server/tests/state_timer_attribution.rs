//! A state timer's move is recorded as the state timer's (ADR-0049).
//!
//! The spec's `on_timeout` follow-on is the app's own rule: Cedar judged the
//! move into the timed state, and the timer then moves the record on. The
//! record's history names the state timer as the one that made that move and
//! keeps the move that armed it as its cause, so it does not read as if the
//! person whose move armed the timer had made it. The timer's move still runs
//! with the arming move's authority: a parameter bound to the authenticated
//! subject still names that person.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::{EntityEvent, STATE_TIMER_ACTOR};
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Gates" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Gate">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="passed_by" Type="Edm.String"/>
      </EntityType>
      <Action Name="Add" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Gate"/></Action>
      <Action Name="Pass" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Gate"/></Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Gates" EntityType="Gates.Gate"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A gate passes itself a second after it is added.
const GATE_IOA: &str = r#"
[automaton]
name = "Gate"
states = ["Unmade", "Closed", "Passed"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Closed", "Passed"]
strict_action_params = true

[[state]]
name = "passed_by"
type = "string"
initial = ""

[[action]]
name = "Add"
from = ["Unmade"]
to = "Closed"
params = []

[[action]]
name = "Note"
from = ["Unmade"]
params = []

[[action]]
name = "Pass"
from = ["Closed"]
to = "Passed"
params = [{ name = "passed_by", type = "string", source = "authenticated_subject" }]

[[state_timeout]]
state = "Closed"
after_seconds = 1
on_timeout = "Pass"
"#;

/// Anyone may add a gate; only the owner may pass one.
const POLICY: &str = r#"
permit(principal, action in [Action::"create", Action::"read", Action::"Add", Action::"Pass"], resource is Gate);
forbid(principal, action == Action::"Pass", resource is Gate) unless { principal.id == "owner" };
"#;

/// A gate whose timer's move calls an outside service through an adapter,
/// which answers with the move that finishes the pass.
fn notifying_gate_ioa(url: &str) -> String {
    format!(
        r#"
[automaton]
name = "Gate"
states = ["Unmade", "Closed", "Passing", "Passed", "Stuck"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Closed", "Passing", "Passed", "Stuck"]

[[action]]
name = "Add"
from = ["Unmade"]
to = "Closed"
params = []

[[action]]
name = "Pass"
kind = "input"
from = ["Closed"]
to = "Passing"

[[action.triggers]]
name = "notify"
kind = "adapter"
adapter = "http"
on_success = "Notified"
on_failure = "NotifyFailed"
config = {{ url = "{url}/notify", method = "POST" }}

[[action]]
name = "Notified"
kind = "input"
from = ["Passing"]
to = "Passed"
params = ["result"]

[[action]]
name = "NotifyFailed"
kind = "input"
from = ["Passing"]
to = "Stuck"
params = ["error_message"]

[[state_timeout]]
state = "Closed"
after_seconds = 1
on_timeout = "Pass"
"#
    )
}

fn build_state(gate_ioa: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Gate", gate_ioa)],
    );
    let state = ServerState::from_registry(ActorSystem::new("state-timer-attribution"), registry);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Gate",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-23T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("policy should load");
    state
}

/// `POST path` with `body` as `principal`.
async fn post(state: &ServerState, path: &str, body: &str, principal: &str) -> StatusCode {
    let mut request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_verified_jwt(
                principal,
                PrincipalKind::Agent,
                Some("test"),
                None,
                None,
                None,
            ),
        ));
    build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

/// The gate's status, `passed_by` and events once it leaves `Closed` and
/// `Passing`, or after `within`.
async fn settle(
    state: &ServerState,
    id: &str,
    within: Duration,
) -> (String, String, Vec<EntityEvent>) {
    let deadline = Instant::now() + within; // determinism-ok: real-time timer test
    loop {
        let entity = state
            .get_tenant_entity_state(&TenantId::default(), "Gate", id)
            .await
            .expect("gate should exist");
        let now = Instant::now(); // determinism-ok: real-time timer test
        let waiting = ["Closed", "Passing"].contains(&entity.state.status.as_str());
        if !waiting || now >= deadline {
            let passed_by = entity.state.fields["passed_by"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            return (
                entity.state.status,
                passed_by,
                entity.state.events.into_iter().collect(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time timer test
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_state_timers_move_is_recorded_as_the_timers_with_the_arming_move_as_its_cause() {
    let state = build_state(GATE_IOA);
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g1"}"#, "member").await,
        StatusCode::CREATED
    );
    let added = post(&state, "/tdata/Gates('g1')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");

    let (status, passed_by, events) = settle(&state, "g1", Duration::from_secs(6)).await;
    assert_eq!(status, "Passed", "the state timer did not move the gate on");
    assert_eq!(
        passed_by, "member",
        "the timer's move runs with the arming move's authority"
    );

    let add = events
        .iter()
        .find(|event| event.action == "Add")
        .expect("the history keeps the member's Add");
    assert_eq!(
        add.origin, None,
        "a caller's own move names no runtime actor or cause"
    );

    let pass = events.last().expect("the history keeps the timer's move");
    assert_eq!(pass.action, "Pass");
    let origin = pass
        .origin
        .as_ref()
        .expect("the timer's move records who made it and what led to it");
    assert_eq!(
        origin.actor, STATE_TIMER_ACTOR,
        "the history says the state timer made the move"
    );
    let cause = &origin.caused_by;
    assert_eq!(cause.agent_id.as_deref(), Some("member"));
    assert_eq!(cause.action.as_deref(), Some("Add"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_state_timers_move_calls_out_as_the_arming_agent_and_its_follow_ons_name_no_timer() {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/notify"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "callback_params": { "result": "ok" } })),
        )
        .mount(&service)
        .await;
    let state = build_state(&notifying_gate_ioa(&service.uri()));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g2"}"#, "member").await,
        StatusCode::CREATED
    );
    let added = post(&state, "/tdata/Gates('g2')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");

    let (status, _, events) = settle(&state, "g2", Duration::from_secs(8)).await;
    assert_eq!(
        status, "Passed",
        "the timer's move or its callback did not run"
    );

    let calls = service.received_requests().await.unwrap_or_default();
    assert_eq!(calls.len(), 1, "the timer's move calls the service once");
    let call: serde_json::Value =
        serde_json::from_slice(&calls[0].body).expect("the adapter sends JSON");
    assert_eq!(
        call["agent_ctx"]["agent_id"], "member",
        "the timer's move calls out as the agent whose move armed it: {call}"
    );

    let pass = events
        .iter()
        .find(|event| event.action == "Pass")
        .expect("the history keeps the timer's move");
    let origin = pass
        .origin
        .as_ref()
        .expect("the timer's move names its maker");
    assert_eq!(origin.actor, STATE_TIMER_ACTOR);
    assert_eq!(origin.caused_by.action.as_deref(), Some("Add"));
    let notified = events
        .iter()
        .find(|event| event.action == "Notified")
        .expect("the history keeps the service's answer");
    assert_eq!(
        notified.origin, None,
        "the move the service's answer makes is its own, not the timer's"
    );
}

/// The observe API's history response for a gate, read as the system.
async fn history_response(state: &ServerState, id: &str) -> (StatusCode, serde_json::Value) {
    let mut request = Request::get(format!("/observe/entities/Gate/{id}/history"))
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::system(),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

/// The record's history from the observe API, read as the system.
async fn history(state: &ServerState, id: &str) -> Vec<serde_json::Value> {
    let (status, json) = history_response(state, id).await;
    assert_eq!(status, StatusCode::OK);
    json["events"].as_array().cloned().unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timers_cause_names_the_history_entry_of_the_arming_move() {
    let mut state = build_state(GATE_IOA);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(80), None));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g3"}"#, "member").await,
        StatusCode::CREATED
    );
    // More moves than the record keeps in memory, so its oldest events are
    // only in the journal when the timer fires.
    let kept_in_memory = temper_server::entity_actor::types::recent_events_budget();
    for _ in 0..kept_in_memory + 10 {
        let noted = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Gate",
                "g3",
                "Note",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("a note is recorded");
        assert!(noted.success, "{:?}", noted.error);
    }
    let added = post(&state, "/tdata/Gates('g3')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");
    let (status, _, _) = settle(&state, "g3", Duration::from_secs(6)).await;
    assert_eq!(status, "Passed", "the state timer did not move the gate on");

    let events = history(&state, "g3").await;
    let pass = events
        .iter()
        .find(|event| event["action"] == "Pass")
        .expect("the history shows the timer's move");
    let cause = &pass["caused_by"]["sequence_nr"];
    let named = events
        .iter()
        .find(|event| &event["sequence"] == cause)
        .unwrap_or_else(|| panic!("no history entry has the cause's sequence {cause}"));
    assert_eq!(
        named["action"], "Add",
        "the timer's cause names the arming move's entry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loaded_records_history_shows_the_same_point_in_time_as_its_state() {
    let journal = SimEventStore::no_faults(81);
    let mut state = build_state(GATE_IOA);
    state.set_storage_stack(StorageStack::from_sim(journal.clone(), None));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g4"}"#, "member").await,
        StatusCode::CREATED
    );

    // The history request reads the actor's state, then the journal. A move
    // commits between those two reads.
    let mut held = journal.hold_next_read("default:Gate:g4");
    let reader = tokio::spawn({
        let state = state.clone();
        async move { history_response(&state, "g4").await }
    });
    held.reached().await;
    let added = post(&state, "/tdata/Gates('g4')/Gates.Add", "{}", "member").await;
    assert!(added.is_success(), "the member's Add was refused: {added}");
    held.release();

    let (status, body) = reader.await.expect("the history request finishes");
    assert_eq!(status, StatusCode::OK);
    let events = body["events"].as_array().cloned().unwrap_or_default();
    let last = events
        .last()
        .expect("the history shows the record's Created");
    assert_eq!(
        last["to_state"], body["current_state"],
        "the history's newest move must end in the state the same response shows: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_history_whose_journal_cannot_be_read_says_so() {
    let journal = SimEventStore::no_faults(82);
    let mut state = build_state(GATE_IOA);
    state.set_storage_stack(StorageStack::from_sim(journal.clone(), None));
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g5"}"#, "member").await,
        StatusCode::CREATED
    );

    journal.fail_next_reads("default:Gate:g5", 1);
    let (status, body) = history_response(&state, "g5").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a history that could not read the journal must not number its events itself: {body}"
    );
}
