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
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

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

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Gate", GATE_IOA)],
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

/// The gate's status, `passed_by` and events once it leaves `Closed`, or
/// after `within`.
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
        if entity.state.status != "Closed" || now >= deadline {
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
    let state = build_state();
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
