//! A reaction chain that reaches [`MAX_REACTION_DEPTH`] is reported, not
//! dropped: the stopped reaction is a failed result and an observe event on
//! the entity whose action would have fired it.

use super::*;
use crate::registry::SpecRegistry;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactionDepth" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Ticker">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Tickers" EntityType="Temper.ReactionDepth.Ticker"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Tick` reacts to itself: a loop nothing else ends.
const TICKER_IOA: &str = r#"
[automaton]
name = "Ticker"
states = ["On"]
initial = "On"

[[state]]
name = "ticks"
type = "counter"
initial = 0

[[action]]
name = "Tick"
kind = "input"
from = ["On"]
effect = ["ticks += 1"]

[[action.triggers]]
name = "tick_again"
kind = "entity"
target_entity = "Ticker"
target_action = "Tick"
resolve_target = { kind = "same_id" }
"#;

fn ticker_state() -> crate::ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant("default", csdl, CSDL.to_string(), &[("Ticker", TICKER_IOA)]);
    let state = crate::ServerState::from_registry(ActorSystem::new("reaction-depth"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy loads");
    state.rebuild_reaction_dispatcher();
    state
}

#[tokio::test]
async fn a_looping_chain_stops_at_the_bound_and_says_so() {
    let state = ticker_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("reaction-depth-test");
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Ticker",
            "t1",
            "Tick",
            serde_json::json!({}),
            &agent,
        )
        .await
        .expect("Tick");
    assert!(response.success, "Tick: {:?}", response.error);

    // The caller's Tick and one per reaction up to the bound.
    let ticker = state
        .get_tenant_entity_state(&tenant, "Ticker", "t1")
        .await
        .expect("Ticker state")
        .state;
    assert_eq!(
        ticker.counters.get("ticks").copied(),
        Some(1 + MAX_REACTION_DEPTH as usize)
    );

    let replay = state.entity_observe_replay(tenant.as_str(), "Ticker", "t1", 0);
    let stopped: Vec<_> = replay
        .events
        .iter()
        .filter(|event| event.event_name == "reaction_cascade_stopped")
        .collect();
    assert_eq!(stopped.len(), 1, "the stopped reaction is reported once");
    // A spec trigger's rule is named `{entity}:{action}:{trigger}`.
    assert_eq!(stopped[0].data["rule"], "Ticker:Tick:tick_again");
    assert_eq!(stopped[0].data["target_entity_id"], "t1");
    assert_eq!(stopped[0].data["depth"], MAX_REACTION_DEPTH);
}

#[tokio::test]
async fn a_reaction_past_the_bound_is_a_failed_result() {
    let state = ticker_state();
    let tenant = TenantId::default();
    let dispatcher = state
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher built");
    let results = dispatcher
        .dispatch_reactions(
            &state,
            &tenant,
            "Ticker",
            "t1",
            "Tick",
            "On",
            &serde_json::json!({}),
            MAX_REACTION_DEPTH,
            &AgentContext::for_service("reaction-depth-test"),
        )
        .await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].success);
    assert_eq!(results[0].rule_name, "Ticker:Tick:tick_again");
    assert_eq!(results[0].depth, MAX_REACTION_DEPTH);
    assert!(
        results[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("chain already ran")),
        "{results:?}"
    );
}
