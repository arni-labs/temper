//! A move's reactions are the ones its tenant's installed specs declare when
//! the move is dispatched, whichever way the specs were installed.
//!
//! Reaction rules come from entity-kind `[[action.triggers]]`. They used to be
//! a cache built from the registry and rebuilt by each install path after it
//! finished (`rebuild_reaction_dispatcher`). A move between an install and its
//! rebuild ran under the new specs with the old rules: a reaction the new
//! version adds did not fire, and one it removes still did. An install path
//! that never rebuilt (the platform deploy) left them stale for good.
//!
//! Here a version is installed into the live registry exactly as an install
//! does it, with no other step, and the next move must follow it.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const SERVICE: &str = "reactions-follow-installed-spec";
const TENANT: &str = "reactions-follow";

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Follow" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Payment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders" EntityType="Temper.Follow.Order"/>
        <EntitySet Name="Payments" EntityType="Temper.Follow.Payment"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Order`, with `triggers` declared on `Confirm`.
fn order_ioa(triggers: &str) -> String {
    format!(
        r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "Confirm"
kind = "input"
from = ["Draft"]
to = "Confirmed"
{triggers}
"#
    )
}

const PAYMENT_IOA: &str = r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "Authorize"
kind = "input"
from = ["Pending"]
to = "Authorized"
"#;

const CONFIRM_AUTHORIZES: &str = r#"
[[action.triggers]]
name = "confirm_authorizes"
kind = "entity"
target_entity = "Payment"
target_action = "Authorize"
resolve_target = { kind = "same_id" }
"#;

const POLICY: &str = r#"
permit(
  principal == Agent::"service:reactions-follow-installed-spec",
  action in [Action::"Confirm", Action::"Authorize"],
  resource
);
"#;

/// Install `order` beside `Payment` into `registry`, as an install does.
fn install(registry: &mut SpecRegistry, order: &str) {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    registry
        .try_register_tenant(
            TENANT,
            csdl,
            CSDL_XML.to_string(),
            &[("Order", order), ("Payment", PAYMENT_IOA)],
        )
        .expect("the version installs");
}

/// A server whose tenant runs `order`.
fn serve(order: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    install(&mut registry, order);
    let state = ServerState::from_registry(ActorSystem::new(SERVICE), registry);
    state
        .authz
        .reload_tenant_policies(TENANT, POLICY)
        .expect("policy loads");
    state
}

/// Install `order` into the running server's registry, and nothing else.
fn hot_install(state: &ServerState, order: &str) {
    install(&mut state.registry.write().unwrap(), order);
}

async fn confirm(state: &ServerState, order: &str) {
    let reply = state
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "Order",
            order,
            "Confirm",
            serde_json::json!({}),
            &AgentContext::for_service(SERVICE),
        )
        .await
        .expect("Confirm is dispatched");
    assert!(reply.success, "Confirm commits: {:?}", reply.error);
}

async fn payment(state: &ServerState, id: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::new(TENANT), "Payment", id)
        .await
        .expect("Payment reads")
        .state
        .status
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_an_installed_version_adds_fires_on_the_next_move() {
    let state = serve(&order_ioa(""));
    confirm(&state, "before").await;
    assert_eq!(payment(&state, "before").await, "Pending");

    hot_install(&state, &order_ioa(CONFIRM_AUTHORIZES));
    confirm(&state, "after").await;
    assert_eq!(
        payment(&state, "after").await,
        "Authorized",
        "the installed version's reaction fires on the first move after the install"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_an_installed_version_removes_no_longer_fires() {
    let state = serve(&order_ioa(CONFIRM_AUTHORIZES));
    confirm(&state, "before").await;
    assert_eq!(payment(&state, "before").await, "Authorized");

    hot_install(&state, &order_ioa(""));
    confirm(&state, "after").await;
    assert_eq!(
        payment(&state, "after").await,
        "Pending",
        "a reaction the installed version no longer declares does not fire"
    );
}
