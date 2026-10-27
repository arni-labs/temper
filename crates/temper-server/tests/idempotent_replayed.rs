//! A bound action repeated with the same `Idempotency-Key` answers as before
//! and says it is a replay: `Idempotent-Replayed: true`. A fresh move carries
//! no such header, so a caller that counts moves (a webhook receiver counting
//! a redelivery) can tell a resend from a new move.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::idempotency::IdempotencyCache;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Replay" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Tally">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Tallies" EntityType="Temper.Replay.Tally"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const TALLY_IOA: &str = r#"
[automaton]
name = "Tally"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Bump"
kind = "input"
from = ["Open"]
effect = ["count += 1"]
"#;

const REPLAYED: &str = "idempotent-replayed";

fn build_state(seed: u64) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Tally", TALLY_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("idempotent-replayed"), registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Tally",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-25T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Value,
}

impl Answer {
    fn replayed(&self) -> Option<&str> {
        self.headers
            .get(REPLAYED)
            .map(|value| value.to_str().expect("header is text"))
    }

    /// The record as a caller reads it from the answer.
    fn record(&self) -> Value {
        json!({
            "status": self.body["status"],
            "counters": self.body["counters"],
            "fields": self.body["fields"],
            "sequence_nr": self.body["sequence_nr"],
            "total_event_count": self.body["total_event_count"],
        })
    }
}

async fn post(state: &ServerState, path: &str, idempotency_key: Option<&str>) -> Answer {
    let body = if path.ends_with("Tallies") {
        json!({"Id": "t1"})
    } else {
        json!({})
    };
    let mut request = Request::post(path).header("Content-Type", "application/json");
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_verified_jwt(
                "alice",
                PrincipalKind::Agent,
                Some("test"),
                None,
                None,
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Answer {
        status,
        headers,
        body,
    }
}

async fn bump(state: &ServerState, key: &str) -> Answer {
    post(state, "/tdata/Tallies('t1')/Temper.Replay.Bump", Some(key)).await
}

async fn open_tally(state: &ServerState) {
    let created = post(state, "/tdata/Tallies", None).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
}

/// The repeat is answered from the bound-action reply cache.
#[tokio::test]
async fn a_repeated_key_says_it_was_replayed_and_a_fresh_move_does_not() {
    let (_guard, _clock, _ids) = install_deterministic_context(301);
    let state = build_state(301);
    open_tally(&state).await;

    let first = bump(&state, "bump-1").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.replayed(), None, "a fresh move is not a replay");
    assert_eq!(first.body["counters"]["count"], 1, "{}", first.body);

    let repeat = bump(&state, "bump-1").await;
    assert_eq!(repeat.status, first.status, "{}", repeat.body);
    assert_eq!(repeat.replayed(), Some("true"), "{:?}", repeat.headers);
    assert_eq!(
        repeat.record(),
        first.record(),
        "the repeat changed nothing"
    );

    let next = bump(&state, "bump-2").await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.body);
    assert_eq!(next.replayed(), None, "a new key is a fresh move");
    assert_eq!(next.body["counters"]["count"], 2, "{}", next.body);
}

/// With every cached reply gone, as after a restart, the record's journal
/// still holds the key: the actor answers from its durable key set.
#[tokio::test]
async fn a_repeated_key_after_the_caches_are_gone_says_it_was_replayed() {
    let (_guard, _clock, _ids) = install_deterministic_context(302);
    let mut state = build_state(302);
    open_tally(&state).await;

    let first = bump(&state, "bump-1").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.replayed(), None, "a fresh move is not a replay");

    // Drop the shared reply cache and the actor holding it; the actor that
    // answers next rebuilds the record from its journal.
    state.idempotency_cache = Arc::new(IdempotencyCache::new());
    let actor_key = format!("{}:Tally:t1", TenantId::default());
    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        temper_runtime::scheduler::sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;
    assert!(
        !state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "the actor holding the cached reply is gone"
    );

    let repeat = bump(&state, "bump-1").await;
    assert_eq!(repeat.status, first.status, "{}", repeat.body);
    assert_eq!(repeat.replayed(), Some("true"), "{:?}", repeat.headers);
    assert_eq!(
        repeat.record(),
        first.record(),
        "the repeat changed nothing"
    );

    let next = bump(&state, "bump-2").await;
    assert_eq!(next.replayed(), None, "a new key is a fresh move");
    assert_eq!(next.body["counters"]["count"], 2, "{}", next.body);
}
