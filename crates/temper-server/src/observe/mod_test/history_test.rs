use super::*;

use crate::entity_actor::types::recent_events_budget;
use crate::storage::{BackendLabel, BoxedEventStore};
use temper_store_sim::SimEventStore;

/// Apply `moves` AddItem actions to one Order.
async fn add_items(state: &ServerState, entity_id: &str, moves: usize) {
    for i in 0..moves {
        let result = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                entity_id,
                "AddItem",
                serde_json::json!({ "ProductId": format!("p{i}") }),
                &AgentContext::default(),
            )
            .await;
        assert!(result.is_ok(), "AddItem {i} failed: {result:?}");
    }
}

fn actor_is_loaded(state: &ServerState, entity_id: &str) -> bool {
    let key = format!("{}:Order:{entity_id}", TenantId::default());
    state.actor_registry.read().unwrap().contains_key(&key)
}

async fn history_response(state: ServerState, uri: &str) -> (StatusCode, serde_json::Value) {
    let app = Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state);
    let response = app.oneshot(system_get(uri)).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// A record whose in-memory trail has been cut short still shows every move
/// it ever took, from sequence 1, while its actor stays loaded.
#[tokio::test]
async fn history_reads_whole_journal_while_actor_is_loaded() {
    let state = test_state_with_turso().await;
    let entity_id = "order-long-trail";
    let moves = recent_events_budget() + 10;
    add_items(&state, entity_id, moves).await;

    assert!(actor_is_loaded(&state, entity_id), "actor stays loaded");
    let memory = state
        .get_tenant_entity_state(&TenantId::default(), "Order", entity_id)
        .await
        .expect("entity state");
    let (store, _) = state.event_journal().expect("event journal");
    let persistence_id = format!("{}:Order:{entity_id}", TenantId::default());
    let journal = store
        .read_events(&persistence_id, 0)
        .await
        .expect("journal readable");
    assert!(journal.len() >= moves, "every move is journaled");
    assert!(
        memory.state.events.len() < journal.len(),
        "precondition: memory holds a partial trail ({} of {})",
        memory.state.events.len(),
        journal.len()
    );

    let (status, json) = history_response(
        state,
        &format!("/observe/entities/Order/{entity_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["current_state"], "Draft", "live state still included");
    let events = json["events"].as_array().expect("events array");
    let sequences: Vec<u64> = events
        .iter()
        .map(|event| event["sequence"].as_u64().expect("sequence"))
        .collect();
    let stored: Vec<u64> = journal.iter().map(|env| env.sequence_nr).collect();
    assert_eq!(
        sequences, stored,
        "history shows the stored sequence numbers"
    );
    assert_eq!(sequences.first(), Some(&1), "history starts at sequence 1");
    for (event, envelope) in events.iter().zip(&journal) {
        assert_eq!(event["action"], envelope.payload["action"]);
    }
}

/// When the journal cannot be read the route says so, instead of answering
/// the partial trail held in memory.
#[tokio::test]
async fn history_store_read_failure_answers_unavailable() {
    let sim = SimEventStore::no_faults(7);
    let mut state = test_state_with_registry();
    state.set_storage_stack(StorageStack::new(
        BackendLabel::Sim,
        BoxedEventStore::new(sim.clone()),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    ));
    let entity_id = "order-unreadable";
    add_items(&state, entity_id, 3).await;
    assert!(actor_is_loaded(&state, entity_id), "actor stays loaded");

    let persistence_id = format!("{}:Order:{entity_id}", TenantId::default());
    sim.fail_next_reads(&persistence_id, 1);
    let (status, _) = history_response(
        state,
        &format!("/observe/entities/Order/{entity_id}/history"),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
