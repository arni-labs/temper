//! Tests for the entity actor's message, state and answer types.

use super::*;
use serde_json::json;

#[test]
fn entity_state_round_trip() {
    let state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "order-1".to_string(),
        status: "Draft".to_string(),
        item_count: 2,
        counters: BTreeMap::from([("items".to_string(), 2)]),
        booleans: BTreeMap::from([("assigned".to_string(), true)]),
        lists: BTreeMap::new(),
        fields: json!({"title": "Test Order"}),
        events: VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        status_clock: Default::default(),
    };
    let serialized = serde_json::to_string(&state).unwrap();
    let deserialized: EntityState = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.entity_type, "Order");
    assert_eq!(deserialized.status, "Draft");
    assert_eq!(deserialized.item_count, 2);
    assert_eq!(deserialized.counters["items"], 2);
    assert!(deserialized.booleans["assigned"]);
}

#[test]
fn entity_state_defaults_on_missing_fields() {
    let json = json!({
        "entity_type": "Task",
        "entity_id": "task-1",
        "status": "Open",
        "item_count": 0,
        "fields": {},
        "events": [],
    });
    let state: EntityState = serde_json::from_value(json).unwrap();
    assert!(state.counters.is_empty());
    assert!(state.booleans.is_empty());
    assert!(state.lists.is_empty());
    assert_eq!(state.total_event_count, 0);
    assert_eq!(state.events_since_snapshot, 0);
    assert_eq!(state.last_snapshot_sequence_nr, 0);
    assert_eq!(state.sequence_nr, 0);
}

#[test]
fn event_budget_is_based_on_snapshot_tail_not_lifetime_total() {
    let state = EntityState {
        entity_type: "Workspace".to_string(),
        entity_id: "workspace-1".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: json!({}),
        events: VecDeque::new(),
        total_event_count: MAX_EVENTS_SINCE_SNAPSHOT + 50,
        events_since_snapshot: 2,
        last_snapshot_sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64 + 48,
        sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64 + 50,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        status_clock: Default::default(),
    };

    assert!(state.can_accept_event());
}

#[test]
fn event_budget_rejects_when_snapshot_tail_reaches_cap() {
    let state = EntityState {
        entity_type: "Workspace".to_string(),
        entity_id: "workspace-1".to_string(),
        status: "Active".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: json!({}),
        events: VecDeque::new(),
        total_event_count: MAX_EVENTS_SINCE_SNAPSHOT,
        events_since_snapshot: MAX_EVENTS_SINCE_SNAPSHOT,
        last_snapshot_sequence_nr: 0,
        sequence_nr: MAX_EVENTS_SINCE_SNAPSHOT as u64,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        status_clock: Default::default(),
    };

    assert!(!state.can_accept_event());
}

#[test]
fn entity_response_spec_governed_default() {
    let json = json!({
        "success": true,
        "state": {
            "entity_type": "Order",
            "entity_id": "o1",
            "status": "Draft",
            "item_count": 0,
            "fields": {},
            "events": [],
        },
        "error": null,
    });
    let resp: EntityResponse = serde_json::from_value(json).unwrap();
    assert!(resp.spec_governed); // default is true
}

#[test]
fn entity_response_spec_governed_skipped_when_true() {
    let state = EntityState {
        entity_type: "Order".to_string(),
        entity_id: "o1".to_string(),
        status: "Draft".to_string(),
        item_count: 0,
        counters: BTreeMap::new(),
        booleans: BTreeMap::new(),
        lists: BTreeMap::new(),
        fields: json!({}),
        events: VecDeque::new(),
        total_event_count: 0,
        events_since_snapshot: 0,
        last_snapshot_sequence_nr: 0,
        sequence_nr: 0,
        processed_idempotency_keys: std::collections::BTreeMap::new(),
        status_clock: Default::default(),
    };
    let resp = EntityResponse {
        success: true,
        refusal: None,
        state,
        error: None,
        custom_effects: vec![],
        scheduled_actions: vec![],
        spawn_requests: vec![],
        spec_governed: true,
        refused: false,
        store_unavailable: false,
    };
    let serialized = serde_json::to_string(&resp).unwrap();
    assert!(!serialized.contains("spec_governed"));
}

#[test]
fn default_spec_governed_is_true() {
    assert!(default_spec_governed());
}

#[test]
fn is_true_helper() {
    assert!(is_true(&true));
    assert!(!is_true(&false));
}
