//! A refused composite sub-write fails the composite and runs its `on_failure`.
//!
//! A Notice's `PutItLive` is a composite whose wasm trigger answers with one
//! sub-write: the Release's `Approve`. When that sub-write is refused, either
//! by Cedar or because the Release has already moved on and its guard no longer
//! allows `Approve`, none of the composite's writes land, and the Notice's
//! declared `on_failure` (`Missed`) runs with the refusal as its `error`.
//! Before, the refusal left the Notice in `Answered` as if the answer had moved
//! the Release. Each case runs with an event journal (the atomic batch) and
//! without one (the per-write path).

use std::time::Duration;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CompositeRefusal" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Notice">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
        <Property Name="error" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Release">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Notices" EntityType="Temper.CompositeRefusal.Notice"/>
        <EntitySet Name="Releases" EntityType="Temper.CompositeRefusal.Release"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// The owner's yes is a composite: its trigger answers with the Release's
/// `Approve`. `Missed` records an answer that could not move the Release.
const NOTICE_IOA: &str = r#"
[automaton]
name = "Notice"
states = ["AsksLive", "Answered", "Closed"]
initial = "AsksLive"
allow_indefinite_states = ["AsksLive", "Answered", "Closed"]
strict_action_params = true

[[state]]
name = "error"
type = "string"
initial = ""

[[action]]
name = "PutItLive"
kind = "composite"
from = ["AsksLive"]
to = "Answered"
params = []

[[action.triggers]]
name = "yes"
kind = "wasm"
module = "answer"
on_failure = "Missed"

[[action.sub_writes]]
target_entity = "Release"
action = "Approve"

[[action]]
name = "Missed"
kind = "input"
from = ["Answered"]
to = "Closed"
params = [{ name = "error", type = "string" }]
"#;

const RELEASE_IOA: &str = r#"
[automaton]
name = "Release"
states = ["Unmade", "Held", "Live", "Out"]
initial = "Unmade"
allow_indefinite_states = ["Unmade", "Held", "Live", "Out"]
strict_action_params = true

[[action]]
name = "Hold"
kind = "input"
from = ["Unmade"]
to = "Held"
params = []

[[action]]
name = "Approve"
kind = "input"
from = ["Held"]
to = "Live"
params = []

[[action]]
name = "Reject"
kind = "input"
from = ["Held"]
to = "Out"
params = []
"#;

const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// Cedar refuses the Release's `Approve`, and allows everything else.
const FORBID_APPROVE: &str = r#"
permit(principal, action, resource);
forbid(principal, action == Action::"Approve", resource is Release);
"#;

const NOTICE: &str = "notice-1";
const RELEASE: &str = "release-1";

/// Whether the server keeps an event journal, which decides how a composite's
/// sub-writes are written.
#[derive(Clone, Copy, Debug)]
enum Journal {
    /// Sub-writes are appended in one atomic batch.
    Atomic,
    /// No journal: sub-writes are dispatched one at a time.
    PerWrite,
}

/// A wasm module that answers with the Release's `Approve` as its sub-write.
fn answer_module() -> String {
    let result = json!({
        "success": true,
        "params": {
            "sub_writes": [{
                "entity_type": "Release",
                "entity_id": RELEASE,
                "action": "Approve",
                "params": {}
            }]
        }
    })
    .to_string();
    let escaped = result.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"(module
             (import "env" "host_set_result" (func $result (param i32 i32)))
             (memory (export "memory") 1)
             (data (i32.const 0) "{escaped}")
             (func (export "run") (param i32 i32) (result i32)
               i32.const 0
               i32.const {len}
               call $result
               i32.const 0))"#,
        len = result.len()
    )
}

fn notice_state(journal: Journal, policy: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("test CSDL should parse"),
        CSDL.to_string(),
        &[("Notice", NOTICE_IOA), ("Release", RELEASE_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("composite-refusal"), registry);
    if let Journal::Atomic = journal {
        state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(29), None));
    }
    let tenant = TenantId::default();
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), policy)
        .expect("test policy should load");
    let hash = state
        .wasm_engine
        .compile_and_cache(answer_module().as_bytes())
        .expect("answer module should compile");
    state.register_wasm_module(&tenant, "answer", &hash);
    state
}

fn agent() -> AgentContext {
    AgentContext::for_service("composite-refusal-test")
}

async fn act(state: &ServerState, entity_type: &str, entity_id: &str, action: &str) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            json!({}),
            &agent(),
        )
        .await
        .unwrap_or_else(|error| panic!("{entity_type}.{action} should dispatch: {error}"));
    assert!(
        response.success,
        "{entity_type}.{action} should apply: {:?}",
        response.error
    );
}

/// The record's status and fields once its status is one of `wanted`, or when
/// the wait runs out.
async fn settle(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    wanted: &[&str],
) -> (String, serde_json::Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let entity = state
            .get_tenant_entity_state(&TenantId::default(), entity_type, entity_id)
            .await
            .expect("record should exist");
        let status = entity.state.status.clone();
        if wanted.contains(&status.as_str()) || tokio::time::Instant::now() >= deadline {
            return (status, entity.state.fields);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn status_of(state: &ServerState, entity_type: &str, entity_id: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::default(), entity_type, entity_id)
        .await
        .expect("record should exist")
        .state
        .status
}

fn error_field(fields: &serde_json::Value) -> String {
    fields
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_moves_the_release_when_nothing_refuses_it() {
    for journal in [Journal::Atomic, Journal::PerWrite] {
        let state = notice_state(journal, PERMIT_ALL);
        act(&state, "Release", RELEASE, "Hold").await;

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (release, _) = settle(&state, "Release", RELEASE, &["Live"]).await;
        assert_eq!(release, "Live", "{journal:?}: the sub-write should apply");
        assert_eq!(
            status_of(&state, "Notice", NOTICE).await,
            "Answered",
            "{journal:?}: an applied composite does not run on_failure"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_refused_by_its_guard_runs_the_composites_on_failure() {
    for journal in [Journal::Atomic, Journal::PerWrite] {
        let state = notice_state(journal, PERMIT_ALL);
        act(&state, "Release", RELEASE, "Hold").await;
        // The Release moves on before the owner answers, so `Approve` no
        // longer applies from its state.
        act(&state, "Release", RELEASE, "Reject").await;

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (notice, fields) = settle(&state, "Notice", NOTICE, &["Closed"]).await;
        assert_eq!(
            notice, "Closed",
            "{journal:?}: the refused sub-write should run Missed"
        );
        let error = error_field(&fields);
        assert!(
            error.contains("sub-write 0") && error.contains("Approve"),
            "{journal:?}: Missed should carry the refusal, got {error:?}"
        );
        assert_eq!(
            status_of(&state, "Release", RELEASE).await,
            "Out",
            "{journal:?}: the refused sub-write leaves the Release as it was"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_write_refused_by_cedar_runs_the_composites_on_failure() {
    for journal in [Journal::Atomic, Journal::PerWrite] {
        let state = notice_state(journal, FORBID_APPROVE);
        act(&state, "Release", RELEASE, "Hold").await;

        act(&state, "Notice", NOTICE, "PutItLive").await;

        let (notice, fields) = settle(&state, "Notice", NOTICE, &["Closed"]).await;
        assert_eq!(
            notice, "Closed",
            "{journal:?}: the denied sub-write should run Missed"
        );
        let error = error_field(&fields);
        assert!(
            error.contains("denied") && error.contains("Release.Approve"),
            "{journal:?}: Missed should carry the denial, got {error:?}"
        );
        assert_eq!(
            status_of(&state, "Release", RELEASE).await,
            "Held",
            "{journal:?}: the denied sub-write leaves the Release as it was"
        );
    }
}
