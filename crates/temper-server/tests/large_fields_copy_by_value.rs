//! A kernel copy of a large field sends its value, not the source's reference.
//!
//! A field too large to keep in its record is stored out of line, and the
//! record holds a reference to it. A write may send only the reference its
//! own record holds (`temper_jit::field_reference`), so a spawn's copied
//! fields and a reaction's args read such a field's value before they send
//! it, and the target stores the value as its own. Before, the copy sent the
//! source's reference and the target's move was refused, while the same
//! copy of a small field went through.

mod common;

use std::time::Duration;

use common::CSDL_XML;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Draft", "Configured", "Fired"]
initial = "Draft"
[[action]]
name = "Configure"
from = ["Draft"]
to = "Configured"
params = ["prompt"]
[[action]]
name = "Fire"
from = ["Configured"]
to = "Fired"
params = ["run"]
effect = ["spawn('Session', 'Start', session_id, params.run, copy(prompt))"]
"#;

const SESSION: &str = r#"
[automaton]
name = "Session"
states = ["Draft", "Ready"]
initial = "Draft"
[[action]]
name = "Start"
from = ["Draft"]
to = "Ready"
params = ["prompt"]
"#;

const SOURCE: &str = r#"
[automaton]
name = "Source"
states = ["Open"]
initial = "Open"
[[action]]
name = "Note"
kind = "input"
from = ["Open"]
params = ["note"]
[[action.triggers]]
name = "mirror"
kind = "entity"
target_entity = "Sink"
target_action = "Copy"
args = { note = "note" }
resolve_target = { kind = "same_id" }
"#;

const SINK: &str = r#"
[automaton]
name = "Sink"
states = ["Empty", "Copied"]
initial = "Empty"
[[action]]
name = "Copy"
kind = "input"
from = ["Empty"]
to = "Copied"
params = ["note"]
"#;

/// Larger than the 128 KiB a record keeps inline.
fn large() -> String {
    "x".repeat(200 * 1024)
}

async fn state_with(dir: &tempfile::TempDir) -> ServerState {
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("copies.db").display()),
        None,
    )
    .await
    .expect("the store opens");
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    registry
        .try_register_tenant(
            "default",
            csdl,
            CSDL_XML.to_string(),
            &[
                ("Job", JOB),
                ("Session", SESSION),
                ("Source", SOURCE),
                ("Sink", SINK),
            ],
        )
        .expect("the tenant registers");
    let mut state = ServerState::from_registry(ActorSystem::new("large-copies"), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state.data_dir = dir.path().to_path_buf();
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("the policy loads");
    state
}

async fn move_on(state: &ServerState, entity_type: &str, id: &str, action: &str, params: Value) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            id,
            action,
            params,
            &Default::default(),
        )
        .await
        .expect("the move answers");
    assert!(response.success, "{action}: {:?}", response.error);
}

/// The stored reference of `field` on a record, proving it went out of line.
async fn stored_reference(state: &ServerState, entity_type: &str, id: &str, field: &str) -> Value {
    let record = state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .expect("the record reads");
    let value = record.state.fields[field].clone();
    assert!(
        value.get("__temper_blob_ref").is_some(),
        "{entity_type} {id}.{field} is stored out of line"
    );
    value
}

async fn reaches(state: &ServerState, entity_type: &str, id: &str, status: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(record) = state
                .get_tenant_entity_state(&TenantId::default(), entity_type, id)
                .await
                && record.state.status == status
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{entity_type} {id} never reached {status}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spawn_copies_a_large_parent_field_by_value() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(&dir).await;
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Job", "job", json!({}))
        .await
        .expect("the job is created");
    move_on(
        &state,
        "Job",
        "job",
        "Configure",
        json!({ "prompt": large() }),
    )
    .await;
    let parent = stored_reference(&state, "Job", "job", "prompt").await;

    move_on(&state, "Job", "job", "Fire", json!({ "run": "session-1" })).await;
    reaches(&state, "Session", "session-1", "Ready").await;
    // Content-addressed: the child stored the same value under the same key.
    assert_eq!(
        stored_reference(&state, "Session", "session-1", "prompt").await,
        parent
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_copies_a_large_field_by_value() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(&dir).await;
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Sink", "s1", json!({}))
        .await
        .expect("the sink is created");
    move_on(&state, "Source", "s1", "Note", json!({ "note": large() })).await;
    let source = stored_reference(&state, "Source", "s1", "note").await;

    reaches(&state, "Sink", "s1", "Copied").await;
    assert_eq!(stored_reference(&state, "Sink", "s1", "note").await, source);
}
