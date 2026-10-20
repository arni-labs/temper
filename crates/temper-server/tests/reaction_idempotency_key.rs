//! A reaction on the source's own record runs when the source request carried
//! an `Idempotency-Key` (ADR-0048).
//!
//! The record records the caller's key for the source action. A reaction that
//! reused that key on the same record was answered as a duplicate and never
//! ran: `SetAudience` with the header left a Room Live instead of moving it to
//! Sharing. A retry of the whole request must still run nothing twice.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const SERVICE: &str = "reaction-key-test";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactKey" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Room">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Rooms" EntityType="Temper.ReactKey.Room"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `SetAudience` stays Live and reacts on the same record with `AdmitGuests`.
const ROOM_IOA: &str = r#"
[automaton]
name = "Room"
states = ["Live", "Sharing"]
initial = "Live"

[[state]]
name = "audiences"
type = "counter"
initial = 0

[[action]]
name = "SetAudience"
kind = "input"
from = ["Live"]
effect = ["audiences += 1"]

[[action.triggers]]
name = "audience_admits_guests"
kind = "entity"
target_entity = "Room"
target_action = "AdmitGuests"
resolve_target = { kind = "same_id" }

[[action]]
name = "AdmitGuests"
kind = "internal"
from = ["Live"]
to = "Sharing"
"#;

fn build_state(tenant: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Room", ROOM_IOA)],
        )
        .expect("tenant registration");
    let state = ServerState::from_registry(ActorSystem::new("reaction-key"), registry);
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal == Agent::"service:reaction-key-test", action, resource is Room);"#,
        )
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
}

fn caller(key: &str) -> AgentContext {
    AgentContext {
        idempotency_key: Some(key.to_string()),
        ..AgentContext::for_service(SERVICE)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_same_record_reaction_runs_when_the_request_carries_an_idempotency_key() {
    let tenant = TenantId::new("rooms");
    let state = build_state("rooms");
    let who = caller("request-1");
    let set_audience = || {
        state.dispatch_tenant_action(
            &tenant,
            "Room",
            "r1",
            "SetAudience",
            serde_json::json!({}),
            &who,
        )
    };

    let first = set_audience().await.expect("SetAudience");
    assert!(first.success, "{:?}", first.error);
    let room = state
        .get_tenant_entity_state(&tenant, "Room", "r1")
        .await
        .expect("room");
    assert_eq!(
        room.state.status, "Sharing",
        "the reaction on the same record ran"
    );
    let events = room.state.total_event_count;

    // The same request again: deduplicated, with its reaction.
    let retry = set_audience().await.expect("retried SetAudience");
    assert!(retry.success, "{:?}", retry.error);
    let room = state
        .get_tenant_entity_state(&tenant, "Room", "r1")
        .await
        .expect("room");
    assert_eq!(
        (
            room.state.status.as_str(),
            room.state.total_event_count,
            room.state.counters["audiences"]
        ),
        ("Sharing", events, 1),
        "a retry runs nothing twice"
    );
}
