//! A spec version that declares new state reaches records made before it.
//!
//! A record made under the first version stores none of the counters and
//! booleans the second declares. Each reads as its declared initial value
//! wherever an action reads it: a constraint, a guard and an effect. The
//! journal then holds the values the action wrote, so a restart reads them
//! back.

mod common;

use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

/// What the second version adds to the Order spec. `brought` must rise with
/// every Bring, as an app's place counter does; `credits` starts at 3 and
/// `open` starts true, so reading either as its type's zero would refuse
/// Spend.
const ADDED: &str = r#"
[[state]]
name = "brought"
type = "counter"
initial = 0

[[state]]
name = "credits"
type = "counter"
initial = 3

[[state]]
name = "open"
type = "bool"
initial = true

[[action]]
name = "Bring"
kind = "input"
from = ["Draft"]
params = ["brought"]
effect = ["brought = params.brought"]

[[action.constraints]]
kind = "param_greater_than_field"
param = "brought"
field = "brought"

[[action]]
name = "Spend"
kind = "input"
from = ["Draft"]
guard = "open && credits > 0"
effect = ["credits -= 1"]
"#;

fn order_v2() -> String {
    format!("{}\n{ADDED}", common::ORDER_IOA)
}

fn install_v2(state: &temper_server::ServerState) {
    let v2 = order_v2();
    let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
    let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    reg.register_tenant(
        "default",
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", v2.as_str())],
    );
}

async fn act(
    state: &temper_server::ServerState,
    id: &str,
    action: &str,
    params: serde_json::Value,
) -> temper_server::entity_actor::EntityResponse {
    let response = common::dispatch(state, &TenantId::default(), "Order", id, action, params)
        .await
        .expect(action);
    assert!(response.success, "{action} on {id}: {:?}", response.error);
    response
}

/// Bring and Spend on a record that stores neither counter, then the values
/// they wrote.
async fn moves_on_an_old_record(state: &temper_server::ServerState, id: &str) {
    let brought = act(state, id, "Bring", serde_json::json!({ "brought": 1 })).await;
    assert_eq!(brought.state.counters.get("brought"), Some(&1));

    let spent = act(state, id, "Spend", serde_json::json!({})).await;
    assert_eq!(spent.state.counters.get("credits"), Some(&2));

    // Once written, the stored value is what the constraint compares.
    let stale = common::dispatch(
        state,
        &TenantId::default(),
        "Order",
        id,
        "Bring",
        serde_json::json!({ "brought": 1 }),
    )
    .await
    .expect("Bring");
    assert!(!stale.success, "a Bring that does not rise is refused");
}

#[tokio::test]
async fn dst_a_live_record_reads_state_a_new_version_declares() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, _store) = common::build_default_state(seed, "dst-spec-adds-state");
        let id = format!("o-{seed}");
        let made = act(&state, &id, "AddItem", serde_json::json!({})).await;
        assert!(!made.state.counters.contains_key("brought"));

        install_v2(&state);
        moves_on_an_old_record(&state, &id).await;
    }
}

#[tokio::test]
async fn dst_a_restarted_record_reads_state_a_new_version_declares() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (first, store) = common::build_default_state(seed, "dst-spec-adds-state-a");
        let id = format!("o-{seed}");
        act(&first, &id, "AddItem", serde_json::json!({})).await;
        drop(first);

        let v2 = order_v2();
        let second = common::build_single_tenant_state_with_store(
            store.clone(),
            "dst-spec-adds-state-b",
            "default",
            &[("Order", v2.as_str())],
        );
        moves_on_an_old_record(&second, &id).await;
        drop(second);

        // The journal holds what the moves wrote.
        let third = common::build_single_tenant_state_with_store(
            store,
            "dst-spec-adds-state-c",
            "default",
            &[("Order", v2.as_str())],
        );
        let after = act(&third, &id, "Spend", serde_json::json!({})).await;
        assert_eq!(after.state.counters.get("brought"), Some(&1));
        assert_eq!(after.state.counters.get("credits"), Some(&1));
    }
}
