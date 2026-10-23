//! A reaction chain stops the first time it would repeat a step, and only
//! then: the stopped reaction is a failed result and an observe event on the
//! entity whose action would have fired it, and a chain that never repeats
//! runs to its end however long it is.

use super::*;
use crate::registry::SpecRegistry;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactionChain" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Ticker">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Node">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="next_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Seed">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Tickers" EntityType="Temper.ReactionChain.Ticker"/>
        <EntitySet Name="Nodes" EntityType="Temper.ReactionChain.Node"/>
        <EntitySet Name="Seeds" EntityType="Temper.ReactionChain.Seed"/>
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

/// `Pass` hands on to the node `next_id` names: the same action, a different
/// entity at every step, so a list of nodes is a chain that never repeats.
const NODE_IOA: &str = r#"
[automaton]
name = "Node"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "passes"
type = "counter"
initial = 0

[[state]]
name = "next_id"
type = "string"
initial = ""

[[action]]
name = "Pass"
kind = "input"
from = ["Ready"]
effect = ["passes += 1"]

[[action.triggers]]
name = "pass_on"
kind = "entity"
target_entity = "Node"
target_action = "Pass"

[action.triggers.resolve_target]
kind = "field"
id_field = "next_id"
"#;

/// `Sprout` makes a new Seed that sprouts in turn: a loop whose every lap is
/// a new entity, so no id ever repeats.
const SEED_IOA: &str = r#"
[automaton]
name = "Seed"
states = ["Sown"]
initial = "Sown"

[[action]]
name = "Sprout"
kind = "input"
from = ["Sown"]

[[action.triggers]]
name = "sprout_again"
kind = "entity"
target_entity = "Seed"
target_action = "Sprout"
resolve_target = { kind = "create" }
"#;

fn chain_state() -> crate::ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL).expect("CSDL parses");
    registry.register_tenant(
        "default",
        csdl,
        CSDL.to_string(),
        &[
            ("Ticker", TICKER_IOA),
            ("Node", NODE_IOA),
            ("Seed", SEED_IOA),
        ],
    );
    let state = crate::ServerState::from_registry(ActorSystem::new("reaction-chain"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy loads");
    state.rebuild_reaction_dispatcher();
    state
}

fn stops(state: &crate::ServerState, entity_type: &str, entity_id: &str) -> Vec<serde_json::Value> {
    state
        .entity_observe_replay(TenantId::default().as_str(), entity_type, entity_id, 0)
        .events
        .into_iter()
        .filter(|event| event.event_name == "reaction_cycle_stopped")
        .map(|event| event.data)
        .collect()
}

async fn run(state: &crate::ServerState, entity_type: &str, entity_id: &str, action: &str) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            serde_json::json!({}),
            &AgentContext::for_service("reaction-chain-test"),
        )
        .await
        .expect("dispatch");
    assert!(response.success, "{action}: {:?}", response.error);
}

/// The old bound let `Tick` run itself eight more times before stopping it.
#[tokio::test]
async fn a_reaction_that_repeats_its_own_step_is_stopped_and_named() {
    let state = chain_state();
    run(&state, "Ticker", "t1", "Tick").await;

    let ticker = state
        .get_tenant_entity_state(&TenantId::default(), "Ticker", "t1")
        .await
        .expect("Ticker state")
        .state;
    assert_eq!(ticker.counters.get("ticks").copied(), Some(1));

    let stopped = stops(&state, "Ticker", "t1");
    assert_eq!(stopped.len(), 1, "the stopped reaction is reported once");
    // A spec trigger's rule is named `{entity}:{action}:{trigger}`.
    assert_eq!(stopped[0]["rule"], "Ticker:Tick:tick_again");
    assert_eq!(stopped[0]["target_entity_id"], "t1");
    assert_eq!(
        stopped[0]["cycle"],
        serde_json::json!(["Ticker(t1).Tick", "Ticker(t1).Tick"])
    );
}

/// The old bound stopped this chain at its ninth node.
#[tokio::test]
async fn a_chain_that_never_repeats_runs_to_its_end() {
    const NODES: usize = 200;
    let state = chain_state();
    let tenant = TenantId::default();
    for index in 0..NODES {
        let fields = if index + 1 < NODES {
            serde_json::json!({ "next_id": format!("n{}", index + 1) })
        } else {
            serde_json::json!({})
        };
        state
            .get_or_create_tenant_entity(&tenant, "Node", &format!("n{index}"), fields)
            .await
            .expect("seed node");
    }

    run(&state, "Node", "n0", "Pass").await;

    for index in 0..NODES {
        let node = state
            .get_tenant_entity_state(&tenant, "Node", &format!("n{index}"))
            .await
            .expect("Node state")
            .state;
        assert_eq!(
            node.counters.get("passes").copied(),
            Some(1),
            "node n{index} did not pass"
        );
        assert!(stops(&state, "Node", &format!("n{index}")).is_empty());
    }
}

/// A loop that makes a new entity every lap never repeats an id; the made
/// entity is known by type and action, so the second lap is the repeat. The
/// old bound let it make eight Seeds.
#[tokio::test]
async fn a_loop_that_makes_an_entity_each_lap_stops_on_its_second_lap() {
    let state = chain_state();
    let tenant = TenantId::default();
    run(&state, "Seed", "s1", "Sprout").await;

    let mut seeds = state.list_entity_ids(&tenant, "Seed");
    seeds.sort();
    assert_eq!(seeds.len(), 2, "s1 and the one Seed it made: {seeds:?}");
    let made = seeds
        .iter()
        .find(|id| id.as_str() != "s1")
        .expect("the made Seed");
    assert!(stops(&state, "Seed", "s1").is_empty());
    let stopped = stops(&state, "Seed", made);
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(
        stopped[0]["cycle"],
        serde_json::json!(["Seed(new).Sprout", "Seed(new).Sprout"])
    );
}

/// A reaction stopped as a repeat is a failed result, not a missing one.
#[tokio::test]
async fn a_stopped_reaction_is_a_failed_result() {
    let state = chain_state();
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
            0,
            &AgentContext::for_service("reaction-chain-test"),
        )
        .await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(!results[0].success);
    assert_eq!(results[0].rule_name, "Ticker:Tick:tick_again");
    assert!(
        results[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Ticker(t1).Tick -> Ticker(t1).Tick")),
        "{results:?}"
    );
}
