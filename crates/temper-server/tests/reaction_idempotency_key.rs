//! Reactions and the caller's `Idempotency-Key` (ADR-0048).
//!
//! A record records the caller's key for the source action. A reaction that
//! reused that key on the same record was answered as a duplicate and never
//! ran: `SetAudience` with the header left a Room Live instead of moving it to
//! Sharing. Each reaction now has a key of its own, derived from the caller's,
//! so a retry of the whole request still runs nothing twice.

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
      <EntityType Name="Door">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Rooms" EntityType="Temper.ReactKey.Room"/>
        <EntitySet Name="Doors" EntityType="Temper.ReactKey.Door"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `SetAudience` reacts on the same Room with `AdmitGuests` and on the Door
/// of the same id with `Open`. Every action counts its runs, so a reaction
/// that ran twice shows.
const ROOM_IOA: &str = r#"
[automaton]
name = "Room"
states = ["Live", "Sharing"]
initial = "Live"

[[state]]
name = "audiences"
type = "counter"
initial = 0

[[state]]
name = "guests"
type = "counter"
initial = 0

[[action]]
name = "SetAudience"
kind = "input"
from = ["Live", "Sharing"]
effect = ["audiences += 1"]

[[action.triggers]]
name = "audience_admits_guests"
kind = "entity"
target_entity = "Room"
target_action = "AdmitGuests"
resolve_target = { kind = "same_id" }

[[action.triggers]]
name = "audience_opens_the_door"
kind = "entity"
target_entity = "Door"
target_action = "Open"
resolve_target = { kind = "same_id" }

[[action]]
name = "AdmitGuests"
kind = "internal"
from = ["Live", "Sharing"]
to = "Sharing"
effect = ["guests += 1"]
"#;

const DOOR_IOA: &str = r#"
[automaton]
name = "Door"
states = ["Shut"]
initial = "Shut"

[[state]]
name = "opened"
type = "counter"
initial = 0

[[action]]
name = "Open"
kind = "input"
from = ["Shut"]
effect = ["opened += 1"]
"#;

fn build_state(tenant: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .try_register_tenant(
            tenant,
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Room", ROOM_IOA), ("Door", DOOR_IOA)],
        )
        .expect("tenant registration");
    let state = ServerState::from_registry(ActorSystem::new("reaction-key"), registry);
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"permit(principal == Agent::"service:reaction-key-test", action, resource);"#,
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

/// `(status, counter)` of one record.
async fn read(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    counter: &str,
) -> (String, usize) {
    let entity = state
        .get_tenant_entity_state(tenant, entity_type, "r1")
        .await
        .expect("record");
    let count = entity.state.counters.get(counter).copied().unwrap_or(0);
    (entity.state.status, count)
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
    assert_eq!(
        read(&state, &tenant, "Room", "guests").await,
        ("Sharing".to_string(), 1),
        "the reaction on the same record ran"
    );
    assert_eq!(read(&state, &tenant, "Door", "opened").await.1, 1);

    // The same request again: its reactions keep their keys, and run nothing
    // twice even though both actions could run again.
    let retry = set_audience().await.expect("retried SetAudience");
    assert!(retry.success, "{:?}", retry.error);
    assert_eq!(read(&state, &tenant, "Room", "audiences").await.1, 1);
    assert_eq!(read(&state, &tenant, "Room", "guests").await.1, 1);
    assert_eq!(read(&state, &tenant, "Door", "opened").await.1, 1);
}
