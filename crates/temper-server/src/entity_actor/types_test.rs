//! Tests for the entity actor types.

use super::*;
use serde_json::json;

#[test]
fn durable_keys_keep_their_action_and_older_snapshots_still_load() {
    let json = json!({
        "entity_type": "Task",
        "entity_id": "task-1",
        "status": "Open",
        "item_count": 0,
        "fields": {},
        "processed_idempotency_keys": {
            "older": 3,
            "newer": { "sequence": 4, "action": "Note" }
        },
    });
    let state: EntityState = serde_json::from_value(json).unwrap();
    let older = &state.processed_idempotency_keys["older"];
    assert_eq!((older.sequence, older.action.as_deref()), (3, None));
    assert!(
        older.was_for("Close"),
        "an older key has no action to compare"
    );
    let newer = &state.processed_idempotency_keys["newer"];
    assert!(newer.was_for("Note"));
    assert!(!newer.was_for("Close"));

    let round_trip: EntityState =
        serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(
        round_trip.processed_idempotency_keys,
        state.processed_idempotency_keys
    );
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
        last_event: Default::default(),
        status_clock: Default::default(),
        spawn_reservations: Default::default(),
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
        last_event: Default::default(),
        status_clock: Default::default(),
        spawn_reservations: Default::default(),
    };

    assert!(!state.can_accept_event());
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
        last_event: Default::default(),
        status_clock: Default::default(),
        spawn_reservations: Default::default(),
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
        answered_action: None,
        trigger_failures: Vec::new(),
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
