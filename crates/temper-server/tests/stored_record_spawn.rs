//! An outside action that spawns a child is checked on the record it stores,
//! including the child's id in the field the spawn writes. The child itself
//! is created by the kernel after the parent commits, so the relation to it
//! is not looked up before then.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Spawn" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="ChildRef" Type="Edm.String" Nullable="true"/>
        <NavigationProperty Name="Child" Type="Temper.Spawn.Child">
          <ReferentialConstraint Property="ChildRef" ReferencedProperty="Id"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="SpawnService">
        <EntitySet Name="Children" EntityType="Temper.Spawn.Child"/>
        <EntitySet Name="Runs" EntityType="Temper.Spawn.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized"]
initial = "New"

[[action]]
name = "Init"
kind = "input"
from = ["New"]
to = "Initialized"
"#;

const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Idle", "Running"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Init', ChildRef, params.child_id)"]

[[field_invariant]]
name = "RunningRunsHaveAChild"
message = "a running run names its child"
assert = "status == 'Running' => ChildRef != null"
"#;

fn verified() -> VerificationStatus {
    VerificationStatus::Completed(EntityVerificationResult {
        all_passed: true,
        levels: vec![EntityLevelSummary {
            level: "L0 SMT".to_string(),
            passed: true,
            summary: "OK".to_string(),
            details: None,
        }],
        verified_at: "2026-10-23T00:00:00Z".to_string(),
    })
}

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        &[("Child", CHILD_IOA), ("Run", RUN_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("stored-record-spawn"), registry);
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Child", "Run"] {
            registry.set_verification_status(&TenantId::default(), entity_type, verified());
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("fixture policy should parse");
    state
}

async fn send(state: &ServerState, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "stored-record-spawn",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn an_action_that_starts_a_child_is_checked_with_the_childs_id() {
    let state = build_state();
    let (status, body) = send(&state, Method::POST, "/tdata/Runs", json!({"Id": "r1"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed r1: {body}");

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.Start",
        json!({"child_id": "c1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let run = state
        .get_tenant_entity_state(&TenantId::default(), "Run", "r1")
        .await
        .expect("run should be readable");
    assert_eq!(run.state.status, "Running");
    assert_eq!(run.state.fields["ChildRef"], "c1");
}
