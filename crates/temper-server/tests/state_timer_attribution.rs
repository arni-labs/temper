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

const BELL_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Gates" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Gate">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Bell">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <Action Name="Add" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Gate"/></Action>
      <Action Name="Pass" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Gate"/></Action>
      <Action Name="Ring" IsBound="true"><Parameter Name="bindingParameter" Type="Gates.Bell"/></Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Gates" EntityType="Gates.Gate"/>
        <EntitySet Name="Bells" EntityType="Gates.Bell"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A gate that passes itself a second after it is added, and whose pass
/// rings the bell of the same id. The reaction declares no principal, so it
/// runs with the authority of the move it reacts to.
const RINGING_GATE_IOA: &str = r#"
[automaton]
name = "Gate"
states = ["Unmade", "Closed", "Passed"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Closed", "Passed"]

[[action]]
name = "Add"
from = ["Unmade"]
to = "Closed"
params = []

[[action]]
name = "Pass"
kind = "input"
from = ["Closed"]
to = "Passed"

[[action.triggers]]
name = "pass_rings_the_bell"
kind = "entity"
target_entity = "Bell"
target_action = "Ring"
resolve_target = { kind = "same_id" }

[[state_timeout]]
state = "Closed"
after_seconds = 1
on_timeout = "Pass"
"#;

const BELL_IOA: &str = r#"
[automaton]
name = "Bell"
states = ["Silent", "Rung"]
initial = "Silent"
allow_indefinite_states = ["Silent", "Rung"]

[[action]]
name = "Ring"
kind = "input"
from = ["Silent"]
to = "Rung"
"#;

/// Anyone may add a gate and ring a bell.
const RINGING_POLICY: &str = r#"
permit(principal, action in [Action::"create", Action::"read", Action::"Add", Action::"Pass"], resource is Gate);
permit(principal, action in [Action::"read", Action::"Ring"], resource is Bell);
"#;

fn build_state(gate_ioa: &str) -> ServerState {
    build_tenant(CSDL_XML, &[("Gate", gate_ioa)], POLICY)
}

/// The default tenant with `specs`, each marked verified, and `policy`.
fn build_tenant(csdl: &str, specs: &[(&str, &str)], policy: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(csdl).expect("CSDL should parse"),
        csdl.to_string(),
        specs,
    );
    let state = ServerState::from_registry(ActorSystem::new("state-timer-attribution"), registry);
    for (entity_type, _) in specs {
        state.registry.write().unwrap().set_verification_status(
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
                verified_at: "2026-10-23T00:00:00Z".to_string(),
            }),
        );
    }
    state
        .authz
        .reload_tenant_policies("default", policy)
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
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

#[path = "state_timer_attribution/history.rs"]
mod history;
#[path = "state_timer_attribution/moves.rs"]
mod moves;
