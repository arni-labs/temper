//! A module computes values; the spec decides the move.
//!
//! A forecast module answers with the chance of rain. The trigger lists
//! `on_success = ["TakeUmbrella", "LeaveUmbrella"]`, and `TakeUmbrella`'s
//! guard reads the value as `params.rain`. The real WASM engine runs each
//! module, and the record's status shows which move the kernel made.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_spec::csdl::parse_csdl;

const DAY_IOA: &str = r#"
[automaton]
name = "Day"
states = ["Idle", "Asked", "Wet", "Dry"]
initial = "Idle"

[[action]]
name = "Ask"
kind = "input"
from = ["Idle"]
to = "Asked"
hint = "Asks the forecast whether to take an umbrella."

[[action.triggers]]
name = "forecast"
kind = "wasm"
module = "forecast"
on_success = ["TakeUmbrella", "LeaveUmbrella"]

[[action]]
name = "TakeUmbrella"
kind = "input"
from = ["Asked"]
to = "Wet"
params = [{ name = "rain", type = "uint64" }]
guard = "params.rain >= 50"
hint = "Rain is likely."

[[action]]
name = "LeaveUmbrella"
kind = "input"
from = ["Asked"]
to = "Dry"
params = [{ name = "rain", type = "uint64" }]
hint = "Rain is unlikely."
"#;

const DAY_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Day" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Day">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Days" EntityType="Temper.Day.Day"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A module that answers `result` whatever it is asked.
fn answering(result: &serde_json::Value) -> String {
    let payload = result.to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
            i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    )
}

/// The status a `Day` settles in after `Ask`, with the forecast module
/// answering `result`.
async fn settles_after_ask(result: serde_json::Value) -> String {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(DAY_CSDL_XML).expect("CSDL parses");
    registry.register_tenant(
        "default",
        csdl,
        DAY_CSDL_XML.to_string(),
        &[("Day", DAY_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("module-answers"), registry);
    let tenant = TenantId::default();
    let hash = state
        .wasm_engine
        .compile_and_cache(answering(&result).as_bytes())
        .expect("the module compiles");
    state.register_wasm_module(&tenant, "forecast", &hash);

    let asked = state
        .dispatch_tenant_action(
            &tenant,
            "Day",
            "today",
            "Ask",
            serde_json::json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("Ask dispatches");
    assert!(asked.success, "Ask is taken: {:?}", asked.error);

    // The answer comes in the background. Wait until the record leaves
    // Asked, or long enough that an answer would have landed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let status = state
            .get_tenant_entity_state(&tenant, "Day", "today")
            .await
            .expect("the record exists")
            .state
            .status;
        if status != "Asked" || tokio::time::Instant::now() >= deadline {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_listed_answer_whose_guard_holds_is_taken() {
    let status = settles_after_ask(serde_json::json!({ "params": { "rain": 70 } })).await;
    assert_eq!(status, "Wet");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_listed_answer_is_taken_when_no_earlier_guard_holds() {
    let status = settles_after_ask(serde_json::json!({ "params": { "rain": 10 } })).await;
    assert_eq!(status, "Dry");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_move_a_module_names_is_ignored_when_the_spec_lists_its_answers() {
    let status = settles_after_ask(serde_json::json!({
        "action": "LeaveUmbrella",
        "params": { "rain": 70 },
    }))
    .await;
    assert_eq!(status, "Wet");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_answer_moves_nothing() {
    let status = settles_after_ask(serde_json::json!({
        "empty": true,
        "params": { "rain": 70 },
    }))
    .await;
    assert_eq!(status, "Asked");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_that_names_no_action_still_sends_on_success() {
    // Modules have long written `"action": ""` to mean "send on_success".
    let status = settles_after_ask(serde_json::json!({
        "action": "",
        "params": { "rain": 10 },
    }))
    .await;
    assert_eq!(status, "Dry");
}
