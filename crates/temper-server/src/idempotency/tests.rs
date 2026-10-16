//! Unit tests for the idempotency cache.

use super::*;
use crate::entity_actor::{EntityEvent, EntityResponse, EntityState};
use std::collections::{BTreeMap, VecDeque};

fn make_response(status: &str) -> EntityResponse {
    EntityResponse {
        success: true,
        state: EntityState {
            entity_type: String::new(),
            entity_id: String::new(),
            status: status.to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({}),
            events: VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        },
        error: None,
        custom_effects: vec![],
        scheduled_actions: vec![],
        spawn_requests: vec![],
        spec_governed: true,
    }
}

/// A busy entity's reply: large fields and a full window of recent
/// events, each carrying the large parameters that wrote them.
fn busy_response(field_bytes: usize) -> EntityResponse {
    let mut response = make_response("Running");
    response.state.fields = serde_json::json!({ "queue": "q".repeat(field_bytes) });
    for index in 0..50u64 {
        response.state.events.push_back(EntityEvent {
            action: "Stepped".to_string(),
            from_status: "Running".to_string(),
            to_status: "Running".to_string(),
            timestamp: sim_now(),
            params: serde_json::json!({ "queue": "q".repeat(field_bytes) }),
            idempotency_key: Some(format!("dispatch:{index}")),
        });
        response
            .state
            .processed_idempotency_keys
            .insert(format!("dispatch:{index}"), index);
    }
    response
}

#[test]
fn put_then_get_returns_cached() {
    let cache = IdempotencyCache::new();
    let resp = make_response("Active");
    cache.put("Order:o1", "key-1", &resp);
    let cached = cache.get("Order:o1", "key-1");
    assert!(cached.is_some());
    assert_eq!(cached.unwrap().state.status, "Active");
}

#[test]
fn pending_effects_do_not_satisfy_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", &make_response("Active"));

    assert!(cache.get("Order:o1", "key-1").is_some());
    assert!(
        cache
            .get_after_effects_applied("Order:o1", "key-1")
            .is_none()
    );

    assert!(cache.mark_effects_applied("Order:o1", "key-1"));
    assert!(
        cache
            .get_after_effects_applied("Order:o1", "key-1")
            .is_some()
    );
}

#[test]
fn put_effects_applied_satisfies_protocol_cache_hit() {
    let cache = IdempotencyCache::new();
    cache.put_effects_applied("Order:o1", "key-1", &make_response("Active"));

    let cached = cache.get_after_effects_applied("Order:o1", "key-1");
    assert_eq!(cached.unwrap().state.status, "Active");
}

#[test]
fn get_missing_returns_none() {
    let cache = IdempotencyCache::new();
    assert!(cache.get("Order:o1", "no-such-key").is_none());
}

#[test]
fn different_actors_isolated() {
    let cache = IdempotencyCache::new();
    cache.put("Order:o1", "key-1", &make_response("A"));
    cache.put("Order:o2", "key-1", &make_response("B"));
    assert_eq!(cache.get("Order:o1", "key-1").unwrap().state.status, "A");
    assert_eq!(cache.get("Order:o2", "key-1").unwrap().state.status, "B");
}

#[test]
fn budget_evicts_oldest_across_actors() {
    let one = estimated_entry_bytes("actor-00", "k-00", &make_response("S"));
    let cache = IdempotencyCache::with_budget_bytes(one * 10);
    for i in 0..25 {
        cache.put(
            &format!("actor-{i:02}"),
            &format!("k-{i:02}"),
            &make_response("S"),
        );
    }
    assert_eq!(cache.held_bytes(), one * 10);
    assert!(
        cache.get("actor-00", "k-00").is_none(),
        "oldest reply evicted"
    );
    assert!(
        cache.get("actor-14", "k-14").is_none(),
        "oldest replies evicted first"
    );
    assert!(
        cache.get("actor-15", "k-15").is_some(),
        "the ten newest replies kept"
    );
    assert!(cache.get("actor-24", "k-24").is_some(), "newest reply kept");
    let entries = cache.entries.read().unwrap();
    assert_eq!(entries.len(), 10);
    assert!(
        !entries.by_actor.contains_key("actor-00"),
        "an actor whose last reply is evicted leaves no map behind"
    );
}

#[test]
fn sustained_writes_hold_the_cache_under_budget() {
    let cache = IdempotencyCache::new();
    for i in 0..2_000 {
        cache.put(
            &format!("Feed:feed-{}", i % 25),
            &format!("dispatch:{i}"),
            &busy_response(40 * 1024),
        );
        assert!(cache.held_bytes() <= IDEMPOTENCY_CACHE_BYTES_BUDGET);
    }
    assert!(cache.get("Feed:feed-24", "dispatch:1999").is_some());
}

#[test]
fn cached_reply_keeps_event_metadata_without_parameters() {
    let cache = IdempotencyCache::new();
    cache.put("Feed:f", "k", &busy_response(1024));
    let cached = cache.get("Feed:f", "k").unwrap();
    assert_eq!(cached.state.events.len(), 50);
    let last = cached.state.events.back().unwrap();
    assert_eq!(last.action, "Stepped");
    assert_eq!(last.from_status, "Running");
    assert!(last.params.is_null());
    assert!(cached.state.processed_idempotency_keys.is_empty());
    assert_eq!(
        cached.state.fields["queue"].as_str().map(str::len),
        Some(1024)
    );
}

#[test]
fn replacing_a_reply_frees_its_bytes() {
    let cache = IdempotencyCache::new();
    cache.put("Feed:f", "k", &busy_response(64 * 1024));
    let first = cache.held_bytes();
    cache.put("Feed:f", "k", &busy_response(64 * 1024));
    assert_eq!(cache.held_bytes(), first);
}

#[test]
fn reply_larger_than_the_budget_is_not_cached() {
    let cache = IdempotencyCache::with_budget_bytes(4 * 1024);
    cache.put("Feed:f", "k", &busy_response(8 * 1024));
    assert!(cache.get("Feed:f", "k").is_none());
    assert_eq!(cache.held_bytes(), 0);
}

#[test]
fn replies_waiting_for_their_effects_are_evicted_last() {
    let one = estimated_entry_bytes("actor-00", "k-00", &make_response("S"));
    let cache = IdempotencyCache::with_budget_bytes(one * 10);
    // The oldest reply is still waiting for its post-dispatch effects: a
    // retry needs it, so newer replies whose effects ran go first.
    cache.put("actor-00", "k-00", &make_response("S"));
    for i in 1..25 {
        cache.put_effects_applied(
            &format!("actor-{i:02}"),
            &format!("k-{i:02}"),
            &make_response("S"),
        );
    }
    assert!(
        cache.get("actor-00", "k-00").is_some(),
        "pending reply kept"
    );
    assert!(
        cache.get("actor-15", "k-15").is_none(),
        "older applied reply evicted"
    );
    assert!(cache.get("actor-24", "k-24").is_some(), "newest reply kept");

    // Once its effects have run it is the oldest applied reply, and goes next.
    assert!(cache.mark_effects_applied("actor-00", "k-00"));
    cache.put_effects_applied("actor-25", "k-25", &make_response("S"));
    assert!(cache.get("actor-00", "k-00").is_none());
    assert_eq!(cache.held_bytes(), one * 10);
}
