//! A kernel copy of a large field sends its value, not the source's reference.
//!
//! A field too large to keep in its record is stored out of line, and the
//! record holds a reference to it. A write may send only the reference its
//! own record holds (`temper_jit::field_reference`), so a spawn's copied
//! fields and a reaction's args read such a field's value before they send
//! it, and the target stores the value as its own. Before, the copy sent the
//! source's reference and the target's move was refused, while the same
//! copy of a small field went through. The value is read in the try that
//! sends the move, so a blob store that does not answer keeps the spawn or
//! the reaction owed, and it lands once the store answers again.

mod common;

use std::time::Duration;

use common::CSDL_XML;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::idempotency::IdempotencyKey;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
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
[[state]]
name = "note"
type = "string"
initial = ""
[[action]]
name = "Note"
kind = "input"
from = ["Open"]
params = ["note"]
[[action]]
name = "Share"
kind = "input"
from = ["Open"]
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
    sent_as(
        state,
        &AgentContext::default(),
        entity_type,
        id,
        action,
        params,
    )
    .await;
}

/// A move sent with a caller's idempotency key: what it owes (a child, a
/// reaction) is kept and tried again when a try does not land, which only a
/// keyed move's follow-ons can be without applying twice.
async fn keyed_move(state: &ServerState, entity_type: &str, id: &str, action: &str) {
    let caller = AgentContext {
        idempotency_key: Some(IdempotencyKey::Request(format!(
            "{entity_type}-{id}-{action}"
        ))),
        ..AgentContext::default()
    };
    let params = match action {
        "Fire" => json!({ "run": id.replace("job", "session") }),
        _ => json!({}),
    };
    sent_as(state, &caller, entity_type, id, action, params).await;
}

async fn sent_as(
    state: &ServerState,
    caller: &AgentContext,
    entity_type: &str,
    id: &str,
    action: &str,
    params: Value,
) {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            id,
            action,
            params,
            caller,
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
    // Long enough for an owed write's second try (1 s, then 2 s).
    tokio::time::timeout(Duration::from_secs(15), async {
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

    move_on(&state, "Source", "s1", "Share", json!({})).await;
    reaches(&state, "Sink", "s1", "Copied").await;
    assert_eq!(stored_reference(&state, "Sink", "s1", "note").await, source);
}

/// The file `reference` names under `dir`, the server's data directory.
fn blob_file(dir: &tempfile::TempDir, reference: &Value) -> std::path::PathBuf {
    let key = reference["__temper_blob_ref"]
        .as_str()
        .expect("a reference has a key");
    let name = key.rsplit('/').next().expect("the key names a file");
    let mut stack = vec![dir.path().to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).expect("the data directory reads") {
            let path = entry.expect("the entry reads").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return path;
            }
        }
    }
    panic!("no blob file {name} under the data directory");
}

/// A blob store that does not answer for `file`: the file becomes a link to
/// itself, which every open fails (not as missing), until it is restored.
struct Unreadable {
    file: std::path::PathBuf,
    bytes: Vec<u8>,
}

impl Unreadable {
    fn make(file: std::path::PathBuf) -> Self {
        let bytes = std::fs::read(&file).expect("the blob reads");
        std::fs::remove_file(&file).expect("the blob is removed");
        std::os::unix::fs::symlink(&file, &file).expect("the blob becomes a loop");
        Self { file, bytes }
    }

    fn restore(self) {
        std::fs::remove_file(&self.file).expect("the loop is removed");
        std::fs::write(&self.file, &self.bytes).expect("the blob is written back");
    }
}

async fn status(state: &ServerState, entity_type: &str, id: &str) -> Option<String> {
    state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .ok()
        .map(|record| record.state.status)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spawn_whose_copied_field_cannot_be_read_yet_lands_once_it_can() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(&dir).await;
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Job", "job-2", json!({}))
        .await
        .expect("the job is created");
    move_on(
        &state,
        "Job",
        "job-2",
        "Configure",
        json!({ "prompt": large() }),
    )
    .await;
    let parent = stored_reference(&state, "Job", "job-2", "prompt").await;
    let unreadable = Unreadable::make(blob_file(&dir, &parent));

    keyed_move(&state, "Job", "job-2", "Fire").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_ne!(
        status(&state, "Session", "session-2").await.as_deref(),
        Some("Ready"),
        "the child started without its copied field"
    );

    unreadable.restore();
    reaches(&state, "Session", "session-2", "Ready").await;
    assert_eq!(
        stored_reference(&state, "Session", "session-2", "prompt").await,
        parent
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_whose_copied_field_cannot_be_read_yet_lands_once_it_can() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(&dir).await;
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Sink", "s2", json!({}))
        .await
        .expect("the sink is created");
    move_on(&state, "Source", "s2", "Note", json!({ "note": large() })).await;
    let source = stored_reference(&state, "Source", "s2", "note").await;
    let unreadable = Unreadable::make(blob_file(&dir, &source));

    keyed_move(&state, "Source", "s2", "Share").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        status(&state, "Sink", "s2").await.as_deref(),
        Some("Empty"),
        "the reaction landed without its copied field"
    );

    unreadable.restore();
    reaches(&state, "Sink", "s2", "Copied").await;
    assert_eq!(stored_reference(&state, "Sink", "s2", "note").await, source);
}

/// A move that sends back the reference its record holds, as a client that
/// echoes a record it read does, is no change to that field.
#[tokio::test(flavor = "multi_thread")]
async fn a_move_may_send_back_the_reference_its_record_holds() {
    let dir = tempfile::tempdir().unwrap();
    let state = state_with(&dir).await;
    move_on(&state, "Source", "s3", "Note", json!({ "note": large() })).await;
    let held = stored_reference(&state, "Source", "s3", "note").await;

    move_on(
        &state,
        "Source",
        "s3",
        "Note",
        json!({ "note": held.clone() }),
    )
    .await;
    assert_eq!(stored_reference(&state, "Source", "s3", "note").await, held);
}
