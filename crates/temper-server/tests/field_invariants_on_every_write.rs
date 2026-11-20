//! A `[[field_invariant]]` holds on every write, whoever makes it.
//!
//! Field invariants used to be checked only for writes from outside the
//! kernel, so a reaction, a state timer or a module's answer could store a
//! record its spec forbids: a sweep's follow-on stored 6 quiet readings of 5
//! checked, and every later outside write on the record was refused. Now a
//! field invariant is one of the type's invariants, which the move code checks
//! for every caller, and a field update is held to them too. A record that
//! already breaks one is not stranded by it: a write that keeps it broken or
//! repairs it goes through, and only a write that would break a rule that held
//! is refused.

mod common;

use std::sync::Arc;

use common::CSDL_XML;
use serde_json::json;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicRng, SimActorHandler, install_deterministic_context};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::entity_actor::{EntityActorHandler, EntityResponse};
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const SERVICE: &str = "field-invariant-writes-test";

/// A watched name: an outside `Swept` carries a sweep's counts, and its
/// reaction `Monitored` copies them onto the record, as a module's answer
/// does. Only the field invariant keeps quiet readings among those checked.
const WATCH: &str = r#"
[automaton]
name = "Order"
states = ["Watching"]
initial = "Watching"

[[state]]
name = "checked_n"
type = "int"
initial = 0

[[state]]
name = "quiet_n"
type = "int"
initial = 0

[[action]]
name = "Swept"
kind = "input"
from = ["Watching"]
params = [{ name = "sweep_checked", type = "int" }, { name = "sweep_quiet", type = "int" }]

[[action.triggers]]
name = "monitor"
kind = "entity"
target_entity = "Order"
target_action = "Monitored"
args = { checked_n = "sweep_checked", quiet_n = "sweep_quiet" }
resolve_target = { kind = "same_id" }

[[action]]
name = "Monitored"
kind = "input"
from = ["Watching"]
params = [{ name = "checked_n", type = "int" }, { name = "quiet_n", type = "int" }]

[[action]]
name = "Note"
kind = "input"
from = ["Watching"]
params = ["note"]

[[field_invariant]]
name = "QuietAmongChecked"
message = "quiet readings are among those checked"
assert = "checked_n >= 0 && quiet_n >= 0 && quiet_n <= checked_n"
"#;

const INVARIANT: &str = "QuietAmongChecked";

/// The same spec before the rule existed.
fn watch_without_invariant() -> &'static str {
    WATCH
        .split("[[field_invariant]]")
        .next()
        .expect("the spec has text before its field invariant")
}

fn register(registry: &mut SpecRegistry, spec: &str) {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    registry
        .try_register_tenant("default", csdl, CSDL_XML.to_string(), &[("Order", spec)])
        .expect("the tenant registers");
}

/// A server whose reactions run through the production dispatcher.
fn state_with(spec: &str, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    register(&mut registry, spec);
    let state = ServerState::from_registry(ActorSystem::new(name), registry);
    state
        .authz
        .reload_tenant_policies(
            "default",
            &format!(
                r#"permit(principal == Agent::"service:{SERVICE}", action, resource is Order);"#
            ),
        )
        .expect("the policy loads");
    state
}

async fn dispatch(
    state: &ServerState,
    id: &str,
    action: &str,
    params: serde_json::Value,
) -> EntityResponse {
    state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            id,
            action,
            params,
            &AgentContext::for_service(SERVICE),
        )
        .await
        .unwrap_or_else(|error| panic!("{action} on {id} did not answer: {error}"))
}

async fn counts(state: &ServerState, id: &str) -> (i64, i64) {
    let record = state
        .get_tenant_entity_state(&TenantId::default(), "Order", id)
        .await
        .expect("the record reads")
        .state;
    let read = |name: &str| record.fields[name].as_i64().unwrap_or_default();
    (read("checked_n"), read("quiet_n"))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_cannot_store_a_record_its_field_invariant_forbids() {
    let state = state_with(WATCH, "field-invariant-reaction");
    let swept = dispatch(
        &state,
        "nvda",
        "Swept",
        json!({"sweep_checked": 5, "sweep_quiet": 5}),
    )
    .await;
    assert!(swept.success, "{:?}", swept.error);
    assert_eq!(
        counts(&state, "nvda").await,
        (5, 5),
        "the reaction copied the counts"
    );

    // A second sweep counts more quiet readings than it checked. The outside
    // move breaks nothing; its reaction, a kernel write, would.
    let swept = dispatch(
        &state,
        "nvda",
        "Swept",
        json!({"sweep_checked": 5, "sweep_quiet": 6}),
    )
    .await;
    assert!(swept.success, "{:?}", swept.error);
    assert_eq!(
        counts(&state, "nvda").await,
        (5, 5),
        "the reaction stored a record breaking {INVARIANT}"
    );

    // Outside writes on the record are not refused because of it.
    let noted = dispatch(&state, "nvda", "Note", json!({"note": "still moves"})).await;
    assert!(noted.success, "{:?}", noted.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_move_that_would_break_a_field_invariant_answers_as_a_broken_data_rule() {
    let state = state_with(WATCH, "field-invariant-refusal");
    let response = dispatch(
        &state,
        "msft",
        "Monitored",
        json!({"checked_n": 2, "quiet_n": 3}),
    )
    .await;
    assert!(!response.success, "the move went through");
    let refusal = serde_json::to_value(&response.refusal).expect("a refusal serializes");
    assert_eq!(refusal["code"], "ConstraintViolation", "{refusal}");
    assert_eq!(refusal["details"]["type"], "field_invariant", "{refusal}");
    assert_eq!(refusal["details"]["invariant"], INVARIANT, "{refusal}");
    assert_eq!(refusal["details"]["operation"], "action", "{refusal}");
    assert_eq!(
        response.error.as_deref(),
        Some("quiet readings are among those checked")
    );
    assert_eq!(counts(&state, "msft").await, (0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_update_cannot_break_a_field_invariant() {
    let state = state_with(WATCH, "field-invariant-update");
    let response = dispatch(
        &state,
        "aapl",
        "Monitored",
        json!({"checked_n": 4, "quiet_n": 2}),
    )
    .await;
    assert!(response.success, "{:?}", response.error);

    let refused = state
        .update_tenant_entity_fields(
            &TenantId::default(),
            "Order",
            "aapl",
            json!({"quiet_n": 9}),
            false,
        )
        .await
        .expect_err("the update stored quiet 9 of 4 checked");
    assert_eq!(refused, "quiet readings are among those checked");
    assert_eq!(counts(&state, "aapl").await, (4, 2));

    let response = state
        .update_tenant_entity_fields(
            &TenantId::default(),
            "Order",
            "aapl",
            json!({"quiet_n": 3}),
            false,
        )
        .await
        .expect("the update answers");
    assert!(response.success, "{:?}", response.error);
    assert_eq!(counts(&state, "aapl").await, (4, 3));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_record_already_breaking_a_field_invariant_is_not_stranded() {
    let state = state_with(watch_without_invariant(), "field-invariant-already-broken");
    let response = dispatch(
        &state,
        "old",
        "Monitored",
        json!({"checked_n": 5, "quiet_n": 6}),
    )
    .await;
    assert!(response.success, "{:?}", response.error);

    // The rule arrives, and the record already breaks it.
    register(&mut state.registry.write().unwrap(), WATCH);

    let noted = dispatch(&state, "old", "Note", json!({"note": "moves"})).await;
    assert!(
        noted.success,
        "a record already breaking {INVARIANT} could not move: {:?}",
        noted.error
    );
    let repaired = dispatch(
        &state,
        "old",
        "Monitored",
        json!({"checked_n": 5, "quiet_n": 0}),
    )
    .await;
    assert!(repaired.success, "{:?}", repaired.error);
    assert_eq!(counts(&state, "old").await, (5, 0));

    // Once it holds, it is held to it.
    let broken = dispatch(
        &state,
        "old",
        "Monitored",
        json!({"checked_n": 5, "quiet_n": 7}),
    )
    .await;
    assert!(
        !broken.success,
        "the repaired record broke {INVARIANT} again"
    );
}

/// Random counts over many seeds, through the move code every caller runs.
/// After each move that went through, the record holds to the field
/// invariant; a move that was refused left it as it was.
#[test]
fn random_counts_never_store_a_record_the_field_invariant_forbids() {
    let table = Arc::new(TransitionTable::from_ioa_source(WATCH));
    let mut refused = 0;
    for seed in 0..64 {
        let _guard = install_deterministic_context(seed);
        let mut rng = DeterministicRng::new(seed);
        let mut actor = EntityActorHandler::new("Order", "n1", table.clone());
        for step in 0..32 {
            let checked = (rng.next_u64() % 8) as i64;
            let quiet = (rng.next_u64() % 10) as i64;
            let read = |actor: &EntityActorHandler, name: &str| {
                actor
                    .state_value(name)
                    .and_then(|value| value.as_i64())
                    .unwrap_or_default()
            };
            let before = (read(&actor, "checked_n"), read(&actor, "quiet_n"));
            let params = json!({"checked_n": checked, "quiet_n": quiet}).to_string();
            let result = actor.handle_message("Monitored", &params);
            let after = (read(&actor, "checked_n"), read(&actor, "quiet_n"));
            if result.is_err() {
                refused += 1;
                assert_eq!(
                    after, before,
                    "seed={seed} step={step}: a refusal changed it"
                );
                continue;
            }
            assert!(
                after.1 <= after.0,
                "seed={seed} step={step}: stored quiet {} of {} checked",
                after.1,
                after.0
            );
        }
    }
    assert!(refused > 0, "no seed tried a count the rule forbids");
}
