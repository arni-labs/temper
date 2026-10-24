//! A parameter declared with `source = "dispatched_at"` is set by the runtime
//! to the time of each dispatch, a state timer's included, and a caller cannot
//! supply it (docs/authenticated-action-parameters.md).

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
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
        <Property Name="moved_at" Type="Edm.String"/>
      </EntityType>
      <Action Name="Add" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Gate"/></Action>
      <Action Name="Pass" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Gate"/></Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Gates" EntityType="Gates.Gate"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Every move keeps its own time; a timer passes the gate a second after it
/// is added.
const GATE_IOA: &str = r#"
[automaton]
name = "Gate"
states = ["Unmade", "Closed", "Passed"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Closed", "Passed"]
strict_action_params = true

[[state]]
name = "moved_at"
type = "string"
initial = ""

[[action]]
name = "Add"
from = ["Unmade"]
to = "Closed"
params = [{ name = "moved_at", type = "string", source = "dispatched_at" }]

[[action]]
name = "Pass"
from = ["Closed"]
to = "Passed"
params = [{ name = "moved_at", type = "string", source = "dispatched_at" }]

[[state_timeout]]
state = "Closed"
after_seconds = 1
on_timeout = "Pass"
"#;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Gate", GATE_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("dispatched-at-params"), registry);
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
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state
}

/// `POST path` with `body` as the agent `member`.
async fn post(state: &ServerState, path: &str, body: &str) -> StatusCode {
    let mut request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_verified_jwt(
                "member",
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

/// The `moved_at` an event recorded, checked to be the RFC 3339 UTC form with
/// milliseconds that JavaScript's `toISOString` writes.
fn moved_at(params: &serde_json::Value) -> DateTime<Utc> {
    let text = params["moved_at"]
        .as_str()
        .unwrap_or_else(|| panic!("no moved_at in {params}"));
    assert_eq!(text.len(), "2026-10-23T12:34:56.789Z".len(), "{text}");
    assert!(text.ends_with('Z') && text.as_bytes()[19] == b'.', "{text}");
    DateTime::parse_from_rfc3339(text)
        .unwrap_or_else(|error| panic!("{text}: {error}"))
        .with_timezone(&Utc)
}

#[tokio::test(flavor = "multi_thread")]
async fn each_move_gets_the_time_it_was_dispatched_a_state_timers_move_included() {
    let state = build_state();
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g1"}"#).await,
        StatusCode::CREATED
    );
    let added = post(&state, "/tdata/Gates('g1')/Gates.Add", "{}").await;
    assert!(added.is_success(), "the Add was refused: {added}");

    let deadline = Instant::now() + Duration::from_secs(6); // determinism-ok: real-time timer test
    let entity = loop {
        let entity = state
            .get_tenant_entity_state(&TenantId::default(), "Gate", "g1")
            .await
            .expect("gate should exist");
        let now = Instant::now(); // determinism-ok: real-time timer test
        if entity.state.status != "Closed" || now >= deadline {
            break entity;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time timer test
    };
    assert_eq!(entity.state.status, "Passed");

    let event = |action: &str| {
        entity
            .state
            .events
            .iter()
            .find(|event| event.action == action)
            .unwrap_or_else(|| panic!("no {action} in the history"))
            .clone()
    };
    let add_at = moved_at(&event("Add").params);
    let pass_at = moved_at(&event("Pass").params);
    assert!(
        pass_at - add_at >= chrono::Duration::milliseconds(900),
        "the timer's move keeps the Add's time: Add {add_at}, Pass {pass_at}"
    );
    assert_eq!(
        entity.state.fields["moved_at"],
        event("Pass").params["moved_at"],
        "the record's moved_at is its latest move's"
    );
}

#[tokio::test]
async fn a_caller_cannot_supply_a_dispatched_at_parameter() {
    let state = build_state();
    assert_eq!(
        post(&state, "/tdata/Gates", r#"{"Id":"g2"}"#).await,
        StatusCode::CREATED
    );
    let sent = post(
        &state,
        "/tdata/Gates('g2')/Gates.Add",
        r#"{"moved_at":"2020-01-01T00:00:00.000Z"}"#,
    )
    .await;
    assert!(!sent.is_success(), "a caller-sent moved_at was accepted");
    let entity = state
        .get_tenant_entity_state(&TenantId::default(), "Gate", "g2")
        .await
        .expect("gate should exist");
    assert_eq!(entity.state.status, "Unmade");
}

#[tokio::test]
async fn a_service_move_gets_its_dispatch_time_without_a_caller_identity() {
    let state = build_state();
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Gate",
            "g3",
            "Add",
            serde_json::json!({}),
            &AgentContext::for_service("worker"),
        )
        .await
        .expect("a service may make a move that binds only the dispatch time");
    assert!(response.success, "{:?}", response.error);
    moved_at(&serde_json::json!({ "moved_at": response.state.fields["moved_at"] }));
}
