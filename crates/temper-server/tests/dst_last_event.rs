//! The latest committed event owns record metadata across timers and restarts.
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

use axum::{body::Body, http::Request};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::scheduler::{
    DeterministicIdGen, DeterministicRng, install_sim_context, sim_now,
};
use temper_runtime::{ActorSystem, persistence::EventStore, tenant::TenantId};
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const SPEC: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Waiting", "Expired", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Expired", "Done"]
[[action]]
name = "Start"
from = ["Idle"]
to = "Waiting"
[[action]]
name = "Poke"
from = ["Waiting"]
[[action]]
name = "Expire"
from = ["Waiting"]
to = "Expired"
[[action]]
name = "Finish"
from = ["Waiting", "Expired"]
to = "Done"
[[action]]
name = "React"
from = ["Idle"]
to = "Waiting"
[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Job"
resolve_target = { kind = "same_id" }
target_action = "Finish"
[[state_timeout]]
state = "Waiting"
after_seconds = 180
on_timeout = "Expire"
"#;
const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Jobs" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Job"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
</EntityType>
<Action Name="Start" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="Poke" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<Action Name="React" IsBound="true"><Parameter Name="bindingParameter" Type="Jobs.Job"/></Action>
<EntityContainer Name="Container"><EntitySet Name="Jobs" EntityType="Jobs.Job"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn server(store: &SimEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", SPEC)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Job",
        temper_server::registry::VerificationStatus::Completed(
            temper_server::registry::EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-01-01T00:00:00Z".into(),
            },
        ),
    );
    let mut state = ServerState::from_registry(ActorSystem::new("last-event"), registry);
    state.set_storage_stack(
        StorageStack::from_sim(store.clone(), None).with_entity_rows_in_journal(),
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state.rebuild_reaction_dispatcher();
    state
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}
async fn settle() {
    for _ in 0..600 {
        tokio::task::yield_now().await;
    }
}
async fn request(state: &ServerState, method: &str, uri: &str, body: Value) -> Value {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("ann", "person", None),
        ));
    let app = if uri.starts_with("/observe") {
        axum::Router::new()
            .nest("/observe", temper_server::observe::build_observe_router())
            .with_state(state.clone())
    } else {
        build_router(state.clone())
    };
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert!(status.is_success(), "{method} {uri}: {status}: {value}");
    value
}
fn metadata(row: &Value) -> Value {
    json!([
        row["last_event_at"],
        row["last_event_via"],
        row["last_event_by"]
    ])
}

#[test]
fn dst_latest_event_survives_timers_reactions_and_snapshot_restore() {
    for seed in 0..16 {
        let clock = Arc::new(process_clock::ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(seed)));
        let store = SimEventStore::no_faults(seed);
        let mut rng = DeterministicRng::new(seed);
        let mut expected = Vec::new();
        runtime().block_on(async {
            clock.begin();
            let state = server(&store);
            // A direct authenticated request, including forged kernel properties.
            request(&state, "POST", "/tdata/Jobs", json!({"Id":"timer", "last_event_by":"forged"})).await;
            request(&state, "POST", "/tdata/Jobs('timer')/Jobs.Start", json!({
                "last_event_at":"1900-01-01T00:00:00Z", "last_event_via":"timer", "last_event_by":"mallory"
            })).await;
            let row = request(&state, "GET", "/tdata/Jobs('timer')", Value::Null).await;
            assert_eq!(metadata(&row), json!([sim_now(), "request", "ann"]), "seed {seed}: request");
            assert!(row["fields"].get("last_event_by").is_none(), "forgery is removed");
            // The timer must replace the previous sender and timestamp.
            tokio::time::advance(Duration::from_secs(180)).await;
            settle().await;
            let row = request(&state, "GET", "/tdata/Jobs('timer')", Value::Null).await;
            assert_eq!(row["status"], "Expired", "seed {seed}: timer fired");
            assert_eq!(metadata(&row), json!([sim_now(), "timer", null]), "seed {seed}: timer");
            expected.push(("timer", metadata(&row)));
            let history = request(&state, "GET", "/observe/entities/Job/timer/history", Value::Null).await;
            let events = history["events"].as_array().unwrap();
            assert_eq!(events.last().unwrap()["via"], "timer");
            assert_eq!(events.last().unwrap()["by"], Value::Null);
            assert_eq!(events[1]["via"], "request");
            assert_eq!(events[1]["by"], "ann");
            request(&state, "POST", "/tdata/Jobs", json!({"Id":"request"})).await;
            request(&state, "POST", "/tdata/Jobs('request')/Jobs.Start", json!({})).await;
            for method in ["PATCH", "PUT"] {
                request(&state, method, "/tdata/Jobs('request')", json!({
                    "last_event_at":"1900-01-01T00:00:00Z", "last_event_via":"timer", "last_event_by":"mallory",
                    "moved_by":"app-owned"
                })).await;
                let row = request(&state, "GET", "/tdata/Jobs('request')", Value::Null).await;
                assert_eq!(metadata(&row), json!([sim_now(), "request", "ann"]), "seed {seed}: {method}");
                assert_eq!(row["fields"]["moved_by"], "app-owned");
                assert!(row["fields"].get("last_event_by").is_none());
            }
            let row = request(&state, "GET", "/tdata/Jobs('request')", Value::Null).await;
            expected.push(("request", metadata(&row)));
            tokio::time::advance(Duration::from_secs(1 + rng.next_bound(60) as u64)).await;
            request(&state, "POST", "/tdata/Jobs", json!({"Id":"reaction"})).await;
            request(&state, "POST", "/tdata/Jobs('reaction')/Jobs.React", json!({})).await;
            settle().await;
            let row = request(&state, "GET", "/tdata/Jobs('reaction')", Value::Null).await;
            assert_eq!(row["status"], "Done", "seed {seed}: reaction fired");
            assert_eq!(metadata(&row), json!([sim_now(), "reaction", null]), "seed {seed}: reaction");
            expected.push(("reaction", metadata(&row)));
            let history = request(&state, "GET", "/observe/entities/Job/reaction/history", Value::Null).await;
            let event = history["events"].as_array().unwrap().last().unwrap();
            assert_eq!(event["via"], "reaction");
            assert_eq!(event["by"], Value::Null);
            let rows = request(&state, "GET", "/tdata/Jobs?$orderby=last_event_at%20desc", Value::Null).await;
            assert_eq!(rows["value"][0]["entity_id"], "reaction");
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=last_event_via%20eq%20'timer'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 1);
            assert_eq!(rows["value"][0]["entity_id"], "timer");
            for (query, expected_id) in [
                ("$filter=last_event_by%20eq%20'ann'", "request"),
                ("$filter=last_event_at%20gt%20'2026-01-01T00:03:00Z'", "reaction"),
            ] {
                let rows = request(&state, "GET", &format!("/tdata/Jobs?{query}"), Value::Null).await;
                assert_eq!(rows["value"].as_array().unwrap().len(), 1, "seed {seed}: {query}");
                assert_eq!(rows["value"][0]["entity_id"], expected_id);
            }
            let rows = request(&state, "GET", "/tdata/Jobs?$filter=last_event_at%20eq%20'2026-01-01T01:03:00%2B01:00'", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 2, "equivalent offset is the same instant");
            let rows = request(&state, "GET", "/tdata/Jobs?$select=entity_id,last_event_at,last_event_via,last_event_by", Value::Null).await;
            assert_eq!(rows["value"].as_array().unwrap().len(), 3);
            for row in rows["value"].as_array().unwrap() {
                let expected = expected.iter().find(|(id, _)| row["entity_id"] == *id).unwrap();
                assert_eq!(metadata(row), expected.1);
            }
            // Snapshots omit history: all three values, including ann's identity,
            // must live independently. The reaction record replays its journal.
            for id in ["timer", "request"] {
                let entity = state.get_tenant_entity_state(&TenantId::default(), "Job", id).await.unwrap().state;
                let mut snapshot = serde_json::to_value(&entity).unwrap();
                snapshot["events"] = json!([]);
                store.save_snapshot(&format!("default:Job:{id}"), entity.sequence_nr, &serde_json::to_vec(&snapshot).unwrap()).await.unwrap();
            }
            clock.end(Duration::from_secs(10));
        });
        runtime().block_on(async {
            clock.begin();
            let state = server(&store);
            for (id, expected) in expected {
                // Hydrate on a fresh process, then read via OData.
                let restored = state
                    .get_tenant_entity_state(&TenantId::default(), "Job", id)
                    .await
                    .unwrap()
                    .state;
                assert_eq!(
                    metadata(&serde_json::to_value(&restored).unwrap()),
                    expected,
                    "seed {seed}: recovered state for {id}, independent of the catalog"
                );
                let row =
                    request(&state, "GET", &format!("/tdata/Jobs('{id}')"), Value::Null).await;
                assert_eq!(metadata(&row), expected, "seed {seed}: restored {id}");
            }
            request(&state, "DELETE", "/tdata/Jobs('request')", Value::Null).await;
            let history = request(
                &state,
                "GET",
                "/observe/entities/Job/request/history",
                Value::Null,
            )
            .await;
            let event = history["events"].as_array().unwrap().last().unwrap();
            assert_eq!(event["via"], "request");
            assert_eq!(event["by"], "ann");
            clock.end(Duration::ZERO);
        });
    }
}
