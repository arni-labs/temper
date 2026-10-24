//! How a callback loop ends: inline at its first repeated step, detached by
//! its own data. Real WASM engine, native actors and compensation.
use super::*;

/// Laps the detached loops run: past the 512 the removed hop budget allowed.
const LONG_LOOP: usize = 600;

/// `Job` whose `action` adds a lap while the guard holds, and runs `module`.
fn job_spec(action: &str, params: &str, laps: usize, module: &str, on_success: &str) -> String {
    format!(
        r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "ticks"
type = "counter"
initial = 0
[[action]]
name = "{action}"
from = ["Idle"]
params = [{params}]
guard = "ticks < {laps}"
effect = ["ticks += 1"]

[[action.triggers]]
name = "{module}"
kind = "wasm"
module = "{module}"
{on_success}
"#
    )
}

/// A server running `spec` for `Job`, with `module` registered as `wat`.
fn job_server(system: &str, spec: &str, module: &str, wat: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", spec)],
    );
    let state = ServerState::from_registry(ActorSystem::new(system), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&TenantId::default(), module, &hash);
    state
}

/// A module that succeeds and asks for `Tick`.
fn tick_module() -> String {
    let payload = json!({"action":"Tick","params":{},"success":true}).to_string();
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

async fn start(
    state: &ServerState,
    action: &str,
    params: serde_json::Value,
    await_integration: bool,
) -> temper_server::EntityResponse {
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.dispatch(temper_server::state::DispatchCommand {
            tenant: &TenantId::default(),
            entity_type: "Job",
            entity_id: "job",
            action,
            params,
            agent_ctx: &Default::default(),
            await_integration,
            await_reactions: true,
        }),
    )
    .await
    .expect("the first action did not answer")
    .unwrap()
}

async fn ticks(state: &ServerState) -> usize {
    state
        .get_tenant_entity_state(&TenantId::default(), "Job", "job")
        .await
        .unwrap()
        .state
        .counters["ticks"]
}

/// The data of each observe event called `name`, oldest first.
fn events(state: &ServerState, name: &str) -> Vec<serde_json::Value> {
    state
        .entity_observe_log
        .lock()
        .unwrap()
        .values()
        .flatten()
        .filter(|event| event.event_name == name)
        .map(|event| event.data.clone())
        .collect()
}

/// The callback depths the retained `state_change` events of callbacks report:
/// the observe log keeps an entity's latest events, so these are the loop's
/// last laps. Each is one callback deeper than the one before, up to `deepest`.
fn assert_depths_rise_to(state: &ServerState, deepest: usize) {
    let mut depths = events(state, "state_change")
        .iter()
        .filter_map(|change| change["callback_depth"].as_u64())
        .collect::<Vec<_>>();
    depths.sort_unstable();
    assert_eq!(depths.last().copied(), Some(deepest as u64), "{depths:?}");
    assert!(
        depths.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{depths:?}"
    );
}

/// Wait until an observe event called `name` arrives, and return them all.
async fn first(state: &ServerState, name: &str) -> Vec<serde_json::Value> {
    let arrived = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        loop {
            let found = events(state, name);
            if !found.is_empty() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if let Ok(found) = arrived {
        return found;
    }
    let ticks = ticks(state).await;
    panic!("no {name} event: ticks={ticks}");
}

/// An inline callback that runs the action it is inside of again is a loop:
/// it is refused at its first repeat, naming the loop, long before the spec's
/// own guard would end it. The old depth bound let it run eight more times.
#[tokio::test]
async fn self_callback_stops_at_its_first_repeat_before_the_spec_guard() {
    let _slot = CALLBACK_TEST_SLOT.acquire().await.unwrap();
    let spec = job_spec("Tick", "", 12, "local_job", r#"on_success = "Tick""#);
    let state = job_server("callback-repeat", &spec, "local_job", &tick_module());
    let result = start(&state, "Tick", json!({}), true).await;
    assert!(!result.success);
    assert_eq!(
        ticks(&state).await,
        1,
        "the callback repeating Tick must not run"
    );
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("Job(job).Tick -> Job(job).Tick")),
        "{:?}",
        result.error
    );
}

/// A background callback that runs the action again is a new lap, paced by
/// the integration it waited on. It runs every lap its spec allows, past the
/// 512 the old hop budget allowed, and stops only at the spec's own guard.
/// Each lap's event reports how many callbacks deep the loop is.
#[tokio::test]
async fn detached_callback_loop_runs_past_512_laps_to_its_guard() {
    let _slot = CALLBACK_TEST_SLOT.acquire().await.unwrap();
    let laps = LONG_LOOP + 1;
    let spec = job_spec("Tick", "", laps, "local_job", r#"on_success = "Tick""#);
    let state = job_server("detached-callback-loop", &spec, "local_job", &tick_module());
    let started = std::time::Instant::now();
    assert!(start(&state, "Tick", json!({}), false).await.success);
    // The loop ends when its guard refuses the next lap's callback.
    let refusals = first(&state, "integration_callback_rejected").await;
    assert_eq!(
        ticks(&state).await,
        laps,
        "a callback was refused before the guard: {refusals:?}"
    );
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0]["action"], "Tick");
    assert!(events(&state, "integration_failure_dropped").is_empty());
    assert_depths_rise_to(&state, LONG_LOOP);
    eprintln!(
        "completed {LONG_LOOP} detached callbacks: elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

/// A failing background integration whose compensation runs the same action
/// again loops through compensation. It runs every lap its spec allows, past
/// 512, and stops when the guard refuses the compensating transition.
#[tokio::test]
async fn failed_compensation_loop_runs_past_512_laps_to_its_guard() {
    let _slot = CALLBACK_TEST_SLOT.acquire().await.unwrap();
    let laps = LONG_LOOP + 1;
    let spec = job_spec("Fail", r#""error""#, laps, "failing_job", "");
    let trap = r#"(module (memory (export "memory") 1) (func (export "run") (param i32 i32) (result i32) unreachable))"#;
    let state = job_server("compensation-loop", &spec, "failing_job", trap);
    let first_action = start(&state, "Fail", json!({"error":"local fixture"}), false).await;
    assert!(first_action.success);
    let dropped = first(&state, "integration_failure_dropped").await;
    assert_eq!(
        ticks(&state).await,
        laps,
        "a compensation stopped before the guard: {dropped:?}"
    );
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(dropped[0]["reason"], "compensating transition was rejected");
    assert!(events(&state, "integration_callback_rejected").is_empty());
    assert_depths_rise_to(&state, LONG_LOOP);
    let after = state
        .get_tenant_entity_state(&TenantId::default(), "Job", "job")
        .await
        .unwrap()
        .state;
    assert_eq!(after.counters["ticks"], laps);
    assert_eq!(after.status, "Idle");
}
