//! A module call that changes nothing leaves no row in the call log.
//!
//! A record that polls through a module wrote a log row for every look, and
//! past the tenant's bound another write to prune one, though nothing happened.
//! The call is still counted in `temper_wasm_invocations_total`. A call that
//! failed, or whose answer moved the record, keeps its row: those are what the
//! log is read for.
//!
//! Each case runs on its own record, so no call's answer waits on another's,
//! both with the caller waiting for the call and with it running behind them.
//!
//! Its own test binary, so the global meter provider is set before anything in
//! this process records a metric.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

/// `Look`'s answer changes nothing, `Silent`'s module says it has none,
/// `Finish`'s moves the record, and `Break`'s module fails.
const WATCH: &str = r#"
[automaton]
name = "Watch"
states = ["Watching", "Done"]
initial = "Watching"
allow_indefinite_states = ["Watching", "Done"]
strict_action_params = true

[[action]]
name = "Look"
kind = "input"
from = ["Watching"]
params = []

[[action.triggers]]
name = "look"
kind = "wasm"
module = "answers"
on_success = "Same"

[[action]]
name = "Same"
kind = "input"
from = ["Watching"]
params = []

[[action]]
name = "Silent"
kind = "input"
from = ["Watching"]
params = []

[[action.triggers]]
name = "silent"
kind = "wasm"
module = "says_nothing"
on_success = "Same"

[[action]]
name = "Finish"
kind = "input"
from = ["Watching"]
params = []

[[action.triggers]]
name = "finish"
kind = "wasm"
module = "moves"
on_success = "Moved"

[[action]]
name = "Moved"
kind = "input"
from = ["Watching"]
to = "Done"
params = []

[[action]]
name = "Break"
kind = "input"
from = ["Watching"]
params = []

[[action.triggers]]
name = "break"
kind = "wasm"
module = "fails"
on_failure = "Same"
"#;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Quiet" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Watch">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Watches" EntityType="Temper.Quiet.Watch"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn exporter() -> &'static (InMemoryMetricExporter, SdkMeterProvider) {
    static EXPORTER: OnceLock<(InMemoryMetricExporter, SdkMeterProvider)> = OnceLock::new();
    EXPORTER.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_meter_provider(provider.clone());
        (exporter, provider)
    })
}

/// `temper_wasm_invocations_total` of `trigger_action`, so far.
fn calls(trigger_action: &str) -> u64 {
    let (exporter, provider) = exporter();
    provider.force_flush().expect("flush metrics");
    let mut newest = BTreeMap::new();
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope
                .metrics
                .iter()
                .filter(|m| m.name == "temper_wasm_invocations_total")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in &sum.data_points {
                    let attribute = |key: &str| {
                        point
                            .attributes
                            .iter()
                            .find(|kv: &&KeyValue| kv.key.as_str() == key)
                            .map(|kv| kv.value.as_str().to_string())
                            .unwrap_or_default()
                    };
                    if attribute("trigger_action") == trigger_action {
                        // Cumulative: a later export holds the total so far.
                        newest.insert(attribute("success"), point.value);
                    }
                }
            }
        }
    }
    newest.values().sum()
}

/// A module that gives `payload` as its result.
fn register_module(state: &ServerState, tenant: &TenantId, name: &str, payload: serde_json::Value) {
    let payload = payload.to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module
            (import "env" "host_set_result" (func $result (param i32 i32)))
            (memory (export "memory") 1)
            (data (i32.const 0) "{data}")
            (func (export "run") (param i32 i32) (result i32)
                i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    );
    let hash = state
        .wasm_engine
        .compile_and_cache(wat.as_bytes())
        .expect("the module compiles");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock") // ci-ok: infallible lock
        .register(tenant, name, &hash);
}

async fn watch_server(dir: &tempfile::TempDir, tenant: &TenantId, name: &str) -> ServerState {
    let url = format!("file:{}", dir.path().join("calls.db").display());
    let turso = TursoEventStore::new(&url, None).await.expect("store opens");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.clone(),
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[("Watch", WATCH)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("policy parses");
    let answer = serde_json::json!({"action": "callback", "params": {}, "success": true});
    register_module(&state, tenant, "answers", answer.clone());
    register_module(&state, tenant, "moves", answer);
    register_module(
        &state,
        tenant,
        "says_nothing",
        serde_json::json!({"action": "callback", "params": {}, "success": true, "empty": true}),
    );
    register_module(
        &state,
        tenant,
        "fails",
        serde_json::json!({"action": "callback", "params": {}, "success": false, "error": "no"}),
    );
    state
}

/// The log's rows for `tenant`, as (module, whether the call succeeded).
async fn log_rows(state: &ServerState, tenant: &TenantId) -> Vec<(String, bool)> {
    let mut rows = Vec::new();
    for store in state.metadata_store_for_tenant(tenant.as_str()).await {
        rows.extend(
            store
                .load_recent_wasm_invocations(tenant.as_str(), 1000)
                .await
                .expect("the log reads")
                .into_iter()
                .map(|row| (row.module_name, row.success)),
        );
    }
    rows.sort();
    rows
}

/// Wait up to ten seconds for `done`.
async fn eventually(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done().await {
        assert!(tokio::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn calls_log_only_what_mattered(await_integration: bool) {
    exporter();
    let dir = tempfile::tempdir().unwrap();
    let tenant = TenantId::new(if await_integration { "waits" } else { "behind" });
    let state = watch_server(&dir, &tenant, tenant.as_str()).await;
    let (looks_before, silent_before) = (calls("Look"), calls("Silent"));
    let caller = AgentContext::system();
    let act = async |id: &str, action: &str| {
        let answer = state
            .dispatch_tenant_action_ext(
                &tenant,
                "Watch",
                id,
                action,
                serde_json::json!({}),
                DispatchExtOptions {
                    agent_ctx: &caller,
                    await_integration,
                    await_reactions: await_integration,
                },
            )
            .await
            .unwrap_or_else(|error| panic!("{action} on {id}: {error}"));
        assert!(answer.success, "{action} on {id}: {answer:?}");
    };

    for _ in 0..3 {
        act("quiet", "Look").await;
    }
    act("silent", "Silent").await;
    act("broken", "Break").await;
    act("moved", "Finish").await;

    eventually("every call ran", async || {
        calls("Look") == looks_before + 3 && calls("Silent") == silent_before + 1
    })
    .await;
    eventually("the calls that mattered are in the log", async || {
        log_rows(&state, &tenant).await.len() >= 2
    })
    .await;
    eventually("the record moved", async || {
        state
            .get_tenant_entity_state(&tenant, "Watch", "moved")
            .await
            .is_ok_and(|moved| moved.state.status == "Done")
    })
    .await;
    // A quiet call's row, if one were written, is written about as soon as the
    // calls after it; give it time so that its absence means something.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        log_rows(&state, &tenant).await,
        [("fails".to_string(), false), ("moves".to_string(), true)],
        "only the failed call and the one that moved its record are in the log"
    );
    let quiet = state
        .get_tenant_entity_state(&tenant, "Watch", "quiet")
        .await
        .expect("the record reads");
    assert_eq!(quiet.state.status, "Watching");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_changes_nothing_leaves_no_row_when_the_caller_waits() {
    calls_log_only_what_mattered(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_changes_nothing_leaves_no_row_behind_the_caller() {
    calls_log_only_what_mattered(false).await;
}
