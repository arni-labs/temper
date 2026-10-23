//! Follow-ons the runtime starts by itself after a transition commits.
//!
//! A zero-delay `schedule` and a spawned child's initial action continue
//! their parent's callback lineage and spend one hop each, so a chain that
//! renews itself without waiting ends at `MAX_CALLBACK_HOPS`. A `schedule`
//! that waits on the clock starts a fresh lineage, so a periodic schedule
//! (a cron, a token refresh) keeps cycling however many hops its cycles spend.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::{AgentContext, MAX_CALLBACK_HOPS};
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.FollowOn" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Loop">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Bud">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Cron">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Twin">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Reminder">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Loops" EntityType="Temper.FollowOn.Loop"/>
        <EntitySet Name="Buds" EntityType="Temper.FollowOn.Bud"/>
        <EntitySet Name="Crons" EntityType="Temper.FollowOn.Cron"/>
        <EntitySet Name="Runs" EntityType="Temper.FollowOn.Run"/>
        <EntitySet Name="Twins" EntityType="Temper.FollowOn.Twin"/>
        <EntitySet Name="Reminders" EntityType="Temper.FollowOn.Reminder"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Re-schedules itself with no delay on every step.
const LOOP_SPEC: &str = r#"
[automaton]
name = "Loop"
states = ["Idle", "On"]
initial = "Idle"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Go"
from = ["Idle"]
to = "On"
effect = ["n += 1", "schedule('Tick', 0)"]

[[action]]
name = "Tick"
from = ["On"]
effect = ["n += 1", "schedule('Tick', 0)"]
"#;

/// Each child's initial action spawns another child of the same type.
const BUD_SPEC: &str = r#"
[automaton]
name = "Bud"
states = ["Seed", "Grown"]
initial = "Seed"

[[action]]
name = "Sprout"
from = ["Seed"]
to = "Grown"
effect = ["spawn('Bud', 'Sprout')"]
"#;

/// The shape of a cron job: each cycle calls an integration, whose callback
/// spawns a run and schedules the next cycle one second later.
const CRON_SPEC: &str = r#"
[automaton]
name = "Cron"
states = ["Active", "Failed"]
initial = "Active"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Trigger"
kind = "input"
from = ["Active"]
effect = ["n += 1"]

[[action.triggers]]
name = "compute_next"
kind = "adapter"
adapter = "http"
on_success = "TriggerComplete"
on_failure = "TriggerFailed"
config = { method = "POST", url = "{url}/next" }

[[action]]
name = "TriggerComplete"
kind = "input"
from = ["Active"]
params = ["result"]
effect = ["schedule('Trigger', 1)", "spawn('Run', 'Start')"]

[[action]]
name = "TriggerFailed"
kind = "input"
from = ["Active"]
to = "Failed"
params = ["error_message"]
"#;

const RUN_SPEC: &str = r#"
[automaton]
name = "Run"
states = ["New", "Started"]
initial = "New"

[[action]]
name = "Start"
from = ["New"]
to = "Started"
"#;

/// Schedules the same action twice, for the same moment, on every step.
const TWIN_SPEC: &str = r#"
[automaton]
name = "Twin"
states = ["Idle", "Armed"]
initial = "Idle"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Arm"
from = ["Idle"]
to = "Armed"
effect = ["n += 1", "schedule('Ring', 1)", "schedule('Ring', 1)"]

[[action]]
name = "Ring"
from = ["Armed"]
effect = ["n += 1", "schedule('Ring', 1)", "schedule('Ring', 1)"]
"#;

/// Schedules one action for two different moments.
const REMINDER_SPEC: &str = r#"
[automaton]
name = "Reminder"
states = ["Idle", "Set"]
initial = "Idle"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Set"
from = ["Idle"]
to = "Set"
effect = ["schedule('Ring', 1)", "schedule('Ring', 2)"]

[[action]]
name = "Ring"
from = ["Set"]
effect = ["n += 1"]
"#;

fn build_state(integration_url: &str) -> ServerState {
    let cron_spec = CRON_SPEC.replace("{url}", integration_url);
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[
            ("Loop", LOOP_SPEC),
            ("Bud", BUD_SPEC),
            ("Cron", cron_spec.as_str()),
            ("Run", RUN_SPEC),
            ("Twin", TWIN_SPEC),
            ("Reminder", REMINDER_SPEC),
        ],
    );
    ServerState::from_registry(ActorSystem::new("follow-on-lineage-test"), registry)
}

async fn counter(state: &ServerState, tenant: &TenantId, entity_type: &str, id: &str) -> usize {
    state
        .get_tenant_entity_state(tenant, entity_type, id)
        .await
        .map(|r| r.state.counters.get("n").copied().unwrap_or(0))
        .unwrap_or(0)
}

/// Sample `read` every 100 ms until it holds still for one second (or the
/// deadline passes) and return the last value.
async fn settle<F, Fut>(read: F, deadline: Duration) -> usize
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    let start = tokio::time::Instant::now();
    let mut last = read().await;
    let mut still_since = tokio::time::Instant::now();
    while start.elapsed() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = read().await;
        if now != last {
            last = now;
            still_since = tokio::time::Instant::now();
        } else if still_since.elapsed() >= Duration::from_secs(1) {
            break;
        }
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn an_immediate_self_schedule_ends_at_the_callback_hop_budget() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Loop",
            "l1",
            "Go",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("Go should commit");
    assert!(response.success);

    let n = settle(
        || counter(&state, &tenant, "Loop", "l1"),
        Duration::from_secs(30),
    )
    .await;
    // Go, then one Tick per hop.
    assert_eq!(n, 1 + MAX_CALLBACK_HOPS as usize);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_spawning_chain_ends_at_the_callback_hop_budget() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Bud",
            "root",
            "Sprout",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("Sprout should commit");
    assert!(response.success);

    let buds = settle(
        || async { state.list_entity_ids(&tenant, "Bud").len() },
        Duration::from_secs(30),
    )
    .await;
    // The root, then one child per hop.
    assert_eq!(buds, 1 + MAX_CALLBACK_HOPS as usize);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clocked_schedule_keeps_cycling_past_the_callback_hop_budget() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/next"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "ok" }
        })))
        .mount(&mock_server)
        .await;
    let state = build_state(&mock_server.uri());
    let tenant = TenantId::default();
    // Start where a long-running cron would be after many cycles: two hops
    // left. Each cycle's integration callback spends one.
    let worn = AgentContext {
        callback_hops: MAX_CALLBACK_HOPS - 2,
        ..AgentContext::default()
    };
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Cron",
            "c1",
            "Trigger",
            serde_json::json!({}),
            &worn,
        )
        .await
        .expect("Trigger should commit");
    assert!(response.success);

    let mut cycles = 0;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cycles = counter(&state, &tenant, "Cron", "c1").await;
        if cycles >= 5 {
            break;
        }
    }
    assert!(cycles >= 5, "cron stopped after {cycles} cycles");

    // Every completed cycle's run starts: the run begins from the hop count of
    // the cycle that spawned it, not of the whole cron history.
    let mut started = 0;
    for id in state.list_entity_ids(&tenant, "Run") {
        let run = state
            .get_tenant_entity_state(&tenant, "Run", &id)
            .await
            .expect("spawned run should load");
        if run.state.status == "Started" {
            started += 1;
        }
    }
    assert!(
        started >= cycles - 1,
        "{started} runs started for {cycles} cycles"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn one_action_scheduled_twice_for_the_same_moment_fires_once() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Twin",
            "w1",
            "Arm",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("Arm should commit");
    assert!(response.success);

    // Once a second: Arm, then Ring at about 1, 2, 3 and 4 s. Without the
    // merge the rings double every second (2 + 4 + 8 + 16 by 4.5 s).
    tokio::time::sleep(Duration::from_millis(4_500)).await;
    let n = counter(&state, &tenant, "Twin", "w1").await;
    assert!((4..=6).contains(&n), "Twin committed {n} steps in 4.5 s");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_action_scheduled_for_two_moments_fires_at_both() {
    let state = build_state("http://127.0.0.1:9");
    let tenant = TenantId::default();
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Reminder",
            "r1",
            "Set",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("Set should commit");
    assert!(response.success);

    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert_eq!(counter(&state, &tenant, "Reminder", "r1").await, 2);
}
