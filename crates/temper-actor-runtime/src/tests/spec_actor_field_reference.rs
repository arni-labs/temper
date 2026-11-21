//! A move on a Postgres-held record cannot write a stored-field reference.
//!
//! The Postgres runtime sends an action to the spec actor as a SpecMessage,
//! which copies its params into the record's fields. A reference written
//! there would be read back as the blob it names by a property read, so the
//! actor refuses one as every other runtime does (`temper_jit::field_reference`).
use super::*;
use prost::Message as _;
use serde_json::json;

const NOTED: &str = r#"
[automaton]
name = "Process"
states = ["Idle"]
initial = "Idle"

[[action]]
name = "Note"
kind = "input"
from = ["Idle"]
params = ["note"]
"#;

fn forged() -> serde_json::Value {
    json!({
        "__temper_blob_ref": format!("field-overflow/sha256/{}.json", "a".repeat(64)),
        "__temper_blob_size": 12,
        "__temper_blob_encoding": "json",
    })
}

fn note(action: &str, params: serde_json::Value) -> Message {
    Message {
        id: 1,
        from: None,
        to: ActorHandle::new("reference-test", "Process"),
        message_type: "SpecMessage".into(),
        payload: SpecMessage::with_params(action, params).encode_to_vec(),
        correlation_id: None,
        created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
    }
}

fn context() -> ActorContext {
    ActorContext::new(
        ActorHandle::new("reference-test", "Process"),
        None,
        None,
        Default::default(),
    )
}

#[tokio::test]
async fn a_move_cannot_write_a_reference() {
    let actor = SpecDrivenActor::from_ioa(NOTED).unwrap();
    let mut state = actor.initial_state_for(&ActorHandle::new("reference-test", "Process"));
    let before = state.clone();
    for value in [forged(), json!({"parts": [forged()]})] {
        let refused = actor
            .handle(
                &context(),
                &mut state,
                &note("Note", json!({ "note": value })),
            )
            .await;
        assert!(
            matches!(&refused, Err(ActorError::Rejected(reason)) if reason.contains("stored-field reference")),
            "{refused:?}"
        );
        assert_eq!(state, before, "a refused move leaves the record as it was");
    }

    actor
        .handle(
            &context(),
            &mut state,
            &note("Note", json!({"note": "plain"})),
        )
        .await
        .expect("a plain value is stored");
    let stored: SpecActorState = serde_json::from_slice(&state).unwrap();
    assert_eq!(stored.fields["note"], "plain");
}

#[test]
fn a_create_cannot_write_a_reference() {
    let actor = SpecDrivenActor::from_ioa(NOTED).unwrap();
    assert!(matches!(
        actor.validate_initial_fields(&json!({ "note": forged() })),
        Err(ActorError::Rejected(_))
    ));
    actor
        .validate_initial_fields(&json!({"note": "plain"}))
        .expect("a plain value is created");
}
