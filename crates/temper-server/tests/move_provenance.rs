//! Each move records who made it and the kernel's dispatch path that carried
//! it, and the record's `$history` read returns both with every event.
//!
//! A timer fires as whoever armed it, so its move records that principal with
//! the timer as its source. A WASM module's callback records the module as its
//! source. Events written before the kernel recorded either replay unchanged
//! and read back with both empty; events written now keep every field an
//! earlier release reads.

mod common;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const PERMIT_ALL: &str = "permit(principal, action, resource);";

fn alice() -> SecurityContext {
    SecurityContext::from_verified_jwt(
        "alice",
        PrincipalKind::Agent,
        Some("tester"),
        Some("human-alice"),
        None,
        None,
    )
}

/// One server over an in-memory journal, with `entities` registered and
/// verified for the default tenant and a policy that permits everything.
fn server(csdl: &str, entities: &[(&str, &str)], name: &str) -> (ServerState, SimEventStore) {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(csdl).expect("CSDL should parse"),
        csdl.to_string(),
        entities,
    );
    for (entity_type, _) in entities {
        registry.set_verification_status(
            &TenantId::default(),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-10-24T00:00:00Z".to_string(),
            }),
        );
    }
    let store = SimEventStore::no_faults(77);
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .expect("policy should load");
    (state, store)
}

async fn send(state: &ServerState, mut request: Request<Body>) -> (StatusCode, Value) {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            alice(),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(state: &ServerState, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, request).await
}

async fn history(state: &ServerState, set: &str, id: &str) -> Vec<Value> {
    let request = Request::get(format!("/tdata/{set}('{id}')/$history"))
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(state, request).await;
    assert_eq!(status, StatusCode::OK, "history: {body:?}");
    body["events"].as_array().expect("events").clone()
}

fn event<'a>(events: &'a [Value], action: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["action"] == action)
        .unwrap_or_else(|| panic!("no {action} event in {events:?}"))
}

/// Wait until the record leaves `from`, or give up after `within`.
async fn settle(state: &ServerState, entity_type: &str, id: &str, from: &str, within: Duration) {
    let deadline = Instant::now() + within; // determinism-ok: real-time timer test
    loop {
        let entity = state
            .get_tenant_entity_state(&TenantId::default(), entity_type, id)
            .await
            .expect("record should exist");
        if entity.state.status != from || Instant::now() >= deadline {
            // determinism-ok: real-time timer test
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time timer test
    }
}

const HOLD_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Provenance" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Hold">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Holds" EntityType="Temper.Provenance.Hold"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A hold expires a second after it is made.
const HOLD_IOA: &str = r#"
[automaton]
name = "Hold"
states = ["Waiting", "Expired"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Expired"]

[[action]]
name = "Expire"
from = ["Waiting"]
to = "Expired"

[[state_timeout]]
state = "Waiting"
after_seconds = 1
on_timeout = "Expire"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_fired_move_records_its_armer_and_the_timer() {
    let (state, _store) = server(HOLD_CSDL, &[("Hold", HOLD_IOA)], "provenance-timer");
    let (status, body) = post(&state, "/tdata/Holds", json!({ "Id": "h1" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    settle(&state, "Hold", "h1", "Waiting", Duration::from_secs(10)).await;

    let events = history(&state, "Holds", "h1").await;
    let created = event(&events, "Created");
    assert_eq!(created["source"], "http", "{created:?}");
    assert_eq!(created["made_by"]["id"], "alice", "{created:?}");
    let expired = event(&events, "Expire");
    assert_eq!(expired["source"], "state_timer", "{expired:?}");
    assert_eq!(
        expired["made_by"]["id"], "alice",
        "the timer fires as its armer"
    );
    assert_eq!(
        expired["made_by"]["acting_for"], "human-alice",
        "{expired:?}"
    );
}

/// Pre-built echo integration module: it reports success, so its
/// `on_success` callback is the module's move.
const ECHO_WASM: &[u8] =
    include_bytes!("../../../crates/temper-wasm/tests/fixtures/echo_integration.wasm");

const ECHO_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Provenance" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Echo">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <Action Name="TriggerEcho" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Provenance.Echo"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Echoes" EntityType="Temper.Provenance.Echo"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ECHO_IOA: &str = r#"
[automaton]
name = "Echo"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Pending", "Done", "Failed"]

[[action]]
name = "TriggerEcho"
kind = "input"
from = ["Idle"]
to = "Pending"

[[action.triggers]]
name = "echo_integration"
kind = "wasm"
module = "echo_integration"
on_success = "EchoSucceeded"
on_failure = "EchoFailed"

[[action]]
name = "EchoSucceeded"
kind = "input"
from = ["Pending"]
to = "Done"

[[action]]
name = "EchoFailed"
kind = "input"
from = ["Pending"]
to = "Failed"
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_module_made_move_records_the_module_as_its_source() {
    let (state, _store) = server(ECHO_CSDL, &[("Echo", ECHO_IOA)], "provenance-module");
    let tenant = TenantId::default();
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock") // ci-ok: infallible lock
        .register(&tenant, "echo_integration", &hash);

    let (status, body) = post(&state, "/tdata/Echoes", json!({ "Id": "e1" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let (status, body) = post(
        &state,
        "/tdata/Echoes('e1')/Temper.Provenance.TriggerEcho",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    settle(&state, "Echo", "e1", "Pending", Duration::from_secs(45)).await;

    let events = history(&state, "Echoes", "e1").await;
    let trigger = event(&events, "TriggerEcho");
    assert_eq!(trigger["source"], "http", "{trigger:?}");
    assert_eq!(trigger["made_by"]["id"], "alice", "{trigger:?}");
    let callback = events
        .iter()
        .find(|event| event["action"] == "EchoSucceeded" || event["action"] == "EchoFailed")
        .unwrap_or_else(|| panic!("the module made no move: {events:?}"));
    assert_eq!(callback["source"], "module", "{callback:?}");
    assert!(callback["made_by"]["id"].is_string(), "{callback:?}");
}

/// The fields every release before this one reads from an event payload.
/// Required where that release requires them.
#[derive(serde::Deserialize)]
struct PreviousReleaseEvent {
    action: String,
    from_status: String,
    to_status: String,
    timestamp: chrono::DateTime<chrono::Utc>,
    params: Value,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    kind: temper_runtime::persistence::EventKind,
}

fn old_envelope(sequence_nr: u64, payload: Value) -> PersistenceEnvelope {
    let timestamp = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    PersistenceEnvelope {
        sequence_nr,
        event_type: payload["action"].as_str().unwrap().to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::from_u128(sequence_nr as u128),
            causation_id: uuid::Uuid::from_u128(sequence_nr as u128),
            correlation_id: uuid::Uuid::from_u128(sequence_nr as u128),
            timestamp,
            actor_id: "default:Order:ord-old".to_string(),
        },
    }
}

#[tokio::test]
async fn an_event_written_before_provenance_replays_unchanged() {
    let store = SimEventStore::no_faults(78);
    let pid = "default:Order:ord-old";
    // A journal exactly as an earlier release wrote it: no made_by, no source.
    let old = [
        json!({"action": "Created", "from_status": "", "to_status": "Draft",
               "timestamp": "2026-09-01T00:00:00Z", "params": {}}),
        json!({"action": "AddItem", "from_status": "Draft", "to_status": "Draft",
               "timestamp": "2026-09-01T00:00:01Z",
               "params": {"ProductId": "p-1", "Quantity": 1}}),
        json!({"action": "SubmitOrder", "from_status": "Draft", "to_status": "Submitted",
               "timestamp": "2026-09-01T00:00:02Z",
               "params": {"ShippingAddressId": "a-1", "PaymentMethod": "card"}}),
    ];
    let envelopes: Vec<_> = old
        .iter()
        .enumerate()
        .map(|(i, payload)| old_envelope(i as u64 + 1, payload.clone()))
        .collect();
    store
        .append(pid, 0, &envelopes)
        .await
        .expect("seed journal");

    let state = common::build_default_state_with_store(store.clone(), "provenance-replay");
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .expect("policy should load");
    let replayed = state
        .get_tenant_entity_state(&TenantId::default(), "Order", "ord-old")
        .await
        .expect("replay the old journal");
    assert_eq!(replayed.state.status, "Submitted");
    assert_eq!(replayed.state.counters.get("items"), Some(&1));

    let caller = AgentContext {
        security_ctx: Some(SecurityContext::from_resolved_identity(
            "agent-1", "operator", None,
        )),
        agent_id: Some("agent-1".to_string()),
        ..Default::default()
    };
    let confirmed = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "ord-old",
            "ConfirmOrder",
            json!({}),
            &caller,
        )
        .await
        .expect("dispatch ConfirmOrder");
    assert!(confirmed.success, "{:?}", confirmed.error);

    let events = history(&state, "Orders", "ord-old").await;
    assert_eq!(events.len(), 4, "{events:?}");
    for old_event in &events[..3] {
        assert_eq!(old_event["made_by"], Value::Null, "{old_event:?}");
        assert_eq!(old_event["source"], Value::Null, "{old_event:?}");
    }
    assert_eq!(events[3]["made_by"]["id"], "agent-1", "{:?}", events[3]);

    // The new event keeps every field an earlier release reads.
    let journal = store.read_events(pid, 0).await.expect("read journal");
    let newest = journal.last().expect("the new event");
    assert!(
        newest.payload.get("made_by").is_some(),
        "{:?}",
        newest.payload
    );
    let previous: PreviousReleaseEvent = serde_json::from_value(newest.payload.clone())
        .expect("an earlier release reads the new event");
    assert_eq!(previous.action, "ConfirmOrder");
    assert_eq!(previous.from_status, "Submitted");
    assert_eq!(previous.to_status, "Confirmed");
    assert!(previous.timestamp > old_envelope(3, old[2].clone()).metadata.timestamp);
    assert!(previous.params.is_object(), "{:?}", previous.params);
    assert!(previous.idempotency_key.is_some());
    assert_eq!(
        previous.kind,
        temper_runtime::persistence::EventKind::Transition
    );
}

#[tokio::test]
async fn an_event_cause_round_trips_and_events_without_one_replay() {
    let store = SimEventStore::no_faults(79);
    let pid = "default:Order:ord-cause";
    let cause = json!({
        "rule": "submits",
        "action": "AddItem",
        "entity_type": "Order",
        "entity_id": "ord-cause",
        "depth": 1,
        "timer": {"armed_action": "AddItem", "armed_by": "agent-1"},
    });
    // Two events as an earlier release wrote them, then one a reaction made
    // after the kernel recorded causes.
    let journal = [
        json!({"action": "Created", "from_status": "", "to_status": "Draft",
               "timestamp": "2026-09-01T00:00:00Z", "params": {}}),
        json!({"action": "AddItem", "from_status": "Draft", "to_status": "Draft",
               "timestamp": "2026-09-01T00:00:01Z",
               "params": {"ProductId": "p-1", "Quantity": 1}}),
        json!({"action": "SubmitOrder", "from_status": "Draft", "to_status": "Submitted",
               "timestamp": "2026-09-01T00:00:02Z",
               "params": {"ShippingAddressId": "a-1", "PaymentMethod": "card"},
               "source": "trigger", "cause": cause}),
    ];
    let envelopes: Vec<_> = journal
        .iter()
        .enumerate()
        .map(|(i, payload)| old_envelope(i as u64 + 1, payload.clone()))
        .collect();
    store
        .append(pid, 0, &envelopes)
        .await
        .expect("seed journal");

    let state = common::build_default_state_with_store(store.clone(), "provenance-cause");
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .expect("policy should load");
    let replayed = state
        .get_tenant_entity_state(&TenantId::default(), "Order", "ord-cause")
        .await
        .expect("replay the journal");
    assert_eq!(replayed.state.status, "Submitted");
    assert_eq!(replayed.state.counters.get("items"), Some(&1));

    let events = history(&state, "Orders", "ord-cause").await;
    assert_eq!(events.len(), 3, "{events:?}");
    for old_event in &events[..2] {
        assert_eq!(old_event["cause"], Value::Null, "{old_event:?}");
    }
    assert_eq!(events[2]["cause"], cause, "the cause reads back unchanged");

    // An earlier release reads the event that carries it.
    let previous: PreviousReleaseEvent = serde_json::from_value(journal[2].clone())
        .expect("an earlier release reads an event with a cause");
    assert_eq!(previous.action, "SubmitOrder");
}
