//! Two servers with the same armed self-loop timer share one journal.
//! The losing fire must catch up without executing the action or its hook again.
#[path = "dst_state_timers_follow_state/world.rs"]
mod world;

use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::{AgentContext, StateTimerFire};
use temper_server::state::custom_effects::CustomEffectHandler;
use temper_server::{ServerState, StorageStack};
use temper_store_sim::SimEventStore;
use world::{ProcessClock, SimTimerRows};

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Ready"]
initial = "Idle"
allow_indefinite_states = ["Idle"]

[[action]]
name = "Start"
from = ["Idle"]
to = "Ready"

[[action]]
name = "Check"
from = ["Ready"]
to = "Ready"

[[action.triggers]]
name = "CountFire"
kind = "hook"
hook = "CountFire"

[[state_timeout]]
state = "Ready"
after_seconds = 60
on_timeout = "Check"
reset_on = ["Check"]
"#;
const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Test.Job"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
const TENANT: &str = "temper-system";

#[derive(Default)]
struct Fires(Mutex<BTreeMap<String, usize>>);
impl CustomEffectHandler for Fires {
    fn handle(
        &self,
        effect: &str,
        _: &str,
        id: &str,
        _: &serde_json::Value,
        _: &ServerState,
    ) -> Result<(), String> {
        assert_eq!(effect, "CountFire");
        *self.0.lock().unwrap().entry(id.to_string()).or_default() += 1;
        Ok(())
    }
}

async fn server(
    events: &SimEventStore,
    rows: &Arc<SimTimerRows>,
    fires: &Arc<Fires>,
    name: &str,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        temper_spec::csdl::parse_csdl(CSDL).unwrap(),
        CSDL.to_string(),
        &[("Job", JOB)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(events.clone(), None));
    state.state_timer_rows = Some(rows.clone());
    state.custom_effect_handler = Some(fires.clone());
    state
        .authz
        .reload_tenant_policies(TENANT, "permit(principal, action, resource);")
        .unwrap();
    state.start_runtime().await.unwrap().await.unwrap();
    state
}

async fn settle() {
    for _ in 0..1000 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn dst_two_servers_fire_one_self_loop_per_arm() {
    for seed in 0..16 {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        runtime.block_on(async {
            clock.begin();
            let events = SimEventStore::no_faults(seed);
            let rows = Arc::new(SimTimerRows::new(seed));
            let fires = Arc::new(Fires::default());
            let first = server(&events, &rows, &fires, "old").await;
            let tenant = TenantId::new(TENANT);
            for id in ["a", "b"] {
                first
                    .get_or_create_tenant_entity(&tenant, "Job", id, json!({}))
                    .await
                    .unwrap();
                assert!(
                    first
                        .dispatch_tenant_action(
                            &tenant,
                            "Job",
                            id,
                            "Start",
                            json!({}),
                            &AgentContext::default()
                        )
                        .await
                        .unwrap()
                        .success
                );
            }
            let second = server(&events, &rows, &fires, "new").await;
            // Both actors have the arming sequence before either fire writes.
            for id in ["a", "b"] {
                second
                    .get_tenant_entity_state(&tenant, "Job", id)
                    .await
                    .unwrap();
                events.inject_append_delay(&format!("{TENANT}:Job:{id}"), Duration::from_millis(1));
            }
            settle().await;
            for tick in 1..=3 {
                tokio::time::advance(Duration::from_secs(60)).await;
                settle().await;
                tokio::time::advance(Duration::from_millis(100)).await;
                settle().await;
                for id in ["a", "b"] {
                    let count = events
                        .dump_journal(&format!("{TENANT}:Job:{id}"))
                        .iter()
                        .filter(|e| e.event_type == "Check")
                        .count();
                    assert!(
                        rows.row(TENANT, id).is_some(),
                        "seed {seed}: successor timer row for {id}"
                    );
                    assert_eq!(
                        count, tick,
                        "seed {seed}, record {id}: one journal fire per arm"
                    );
                    assert_eq!(
                        fires.0.lock().unwrap().get(id).copied().unwrap_or(0),
                        tick,
                        "seed {seed}, record {id}: one integration per arm"
                    );
                }
            }
            // A re-fire on an actor that already caught up is dropped too.
            let stale = AgentContext {
                fires_state_timer: Some(StateTimerFire {
                    tenant: TENANT.into(),
                    entity_type: "Job".into(),
                    entity_id: "a".into(),
                    sequence_nr: 0,
                }),
                ..AgentContext::default()
            };
            let response = first
                .dispatch_tenant_action(&tenant, "Job", "a", "Check", json!({}), &stale)
                .await
                .unwrap();
            assert!(!response.success && response.refused);
            assert_eq!(fires.0.lock().unwrap()["a"], 3);

            // The precondition is record-scoped; inherited context cannot
            // suppress a move on another record.
            assert!(
                first
                    .dispatch_tenant_action(&tenant, "Job", "b", "Check", json!({}), &stale)
                    .await
                    .unwrap()
                    .success
            );
            // Caller moves retain ADR-0046 replay and re-evaluation. Both
            // servers can perform a new Check, even with stale local state.
            for state in [&first, &second] {
                assert!(
                    state
                        .dispatch_tenant_action(
                            &tenant,
                            "Job",
                            "a",
                            "Check",
                            json!({}),
                            &AgentContext::default()
                        )
                        .await
                        .unwrap()
                        .success
                );
            }
            assert_eq!(
                events
                    .dump_journal(&format!("{TENANT}:Job:a"))
                    .iter()
                    .filter(|e| e.event_type == "Check")
                    .count(),
                5
            );
            assert_eq!(fires.0.lock().unwrap()["a"], 5);
            clock.end(Duration::ZERO);
        });
    }
}
