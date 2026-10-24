//! Idempotency cache for deduplicating agent retries.
//!
//! Recent `Idempotency-Key` → `EntityResponse` replies, kept per entity actor.
//! Entries expire after `IDEMPOTENCY_TTL_SECS`.
//!
//! Every dispatched action carries a key, so every write adds a reply. The
//! cache is therefore bounded by the bytes it holds across all actors
//! ([`IDEMPOTENCY_CACHE_BYTES_BUDGET`]), and a cached reply leaves out the two
//! things that dominate a busy entity's state: its recent events' parameters
//! and the actor's durable key set. Replies whose post-dispatch effects have
//! run are evicted first, oldest first: the actor's durable
//! `processed_idempotency_keys` still deduplicates them, without the original
//! reply. Replies still waiting for their effects go last, because a retry
//! needs their effects (spawns, timers, triggers) to run once.

use std::collections::BTreeMap;
use std::sync::RwLock;

use temper_runtime::scheduler::sim_now;

use crate::entity_actor::{EntityEvent, EntityResponse, EntityState};

/// Estimated bytes of cached replies held across all actors (TigerStyle budget).
pub const IDEMPOTENCY_CACHE_BYTES_BUDGET: usize = 16 * 1024 * 1024;

/// Time-to-live for idempotency entries in seconds.
pub const IDEMPOTENCY_TTL_SECS: i64 = 3600;

/// Bytes counted for every entry on top of its contents: map nodes, the
/// order index and the reply's fixed fields.
const ENTRY_OVERHEAD_BYTES: usize = 256;
/// Bytes counted for every JSON value, event or list item on top of its text.
const VALUE_OVERHEAD_BYTES: usize = 32;

/// A cached idempotent response.
struct IdempotencyEntry {
    /// The cached response to return on duplicate requests.
    response: EntityResponse,
    /// When this entry was created (for TTL eviction).
    created_at: chrono::DateTime<chrono::Utc>,
    /// Whether dispatcher-side post-dispatch effects have completed for this
    /// cached response.
    effects_applied: bool,
    /// Estimated bytes this entry holds; counts toward the budget.
    bytes: usize,
    /// Position in the cache-wide insertion order.
    seq: u64,
}

/// Insertion order, oldest first: seq → (actor_key, idempotency_key).
type Order = BTreeMap<u64, (String, String)>;

#[derive(Default)]
struct Entries {
    /// actor_key → (idempotency_key → entry).
    by_actor: BTreeMap<String, BTreeMap<String, IdempotencyEntry>>,
    /// Replies whose post-dispatch effects have run; evicted first.
    applied: Order,
    /// Replies still waiting for their post-dispatch effects; evicted last.
    pending: Order,
    next_seq: u64,
    bytes: usize,
}

impl Entries {
    fn len(&self) -> usize {
        self.applied.len() + self.pending.len()
    }

    fn remove(&mut self, actor_key: &str, idem_key: &str) {
        let Some(actor_entries) = self.by_actor.get_mut(actor_key) else {
            return;
        };
        let Some(entry) = actor_entries.remove(idem_key) else {
            return;
        };
        if actor_entries.is_empty() {
            self.by_actor.remove(actor_key);
        }
        self.applied.remove(&entry.seq);
        self.pending.remove(&entry.seq);
        self.bytes -= entry.bytes;
    }

    /// Evict one reply: the oldest whose effects have run, else the oldest.
    fn evict_one(&mut self) -> bool {
        let Some((_, (actor_key, idem_key))) = self
            .applied
            .pop_first()
            .or_else(|| self.pending.pop_first())
        else {
            return false;
        };
        self.remove(&actor_key, &idem_key);
        true
    }

    /// Evict every reply older than the TTL.
    fn evict_expired(&mut self, now: chrono::DateTime<chrono::Utc>) {
        loop {
            let oldest = [
                self.applied.first_key_value(),
                self.pending.first_key_value(),
            ]
            .into_iter()
            .flatten()
            .min_by_key(|(seq, _)| **seq)
            .map(|(_, keys)| keys.clone());
            let Some((actor_key, idem_key)) = oldest else {
                return;
            };
            let expired = self
                .by_actor
                .get(&actor_key)
                .and_then(|actor_entries| actor_entries.get(&idem_key))
                .is_some_and(|entry| is_expired(entry, now));
            if !expired {
                return;
            }
            self.remove(&actor_key, &idem_key);
        }
    }
}

fn is_expired(entry: &IdempotencyEntry, now: chrono::DateTime<chrono::Utc>) -> bool {
    now.signed_duration_since(entry.created_at).num_seconds() > IDEMPOTENCY_TTL_SECS
}

/// Idempotency cache shared by every entity actor.
///
/// Thread-safe via `RwLock`. Uses `BTreeMap` for deterministic iteration
/// order (DST compliance).
pub struct IdempotencyCache {
    entries: RwLock<Entries>,
    budget_bytes: usize,
}

impl IdempotencyCache {
    /// Create a new empty idempotency cache with the default budget.
    pub fn new() -> Self {
        Self::with_budget_bytes(IDEMPOTENCY_CACHE_BYTES_BUDGET)
    }

    /// Create a new empty idempotency cache holding at most `budget_bytes`.
    pub fn with_budget_bytes(budget_bytes: usize) -> Self {
        Self {
            entries: RwLock::new(Entries::default()),
            budget_bytes,
        }
    }

    /// Estimated bytes of cached replies currently held.
    pub fn held_bytes(&self) -> usize {
        match self.entries.read() {
            Ok(entries) => entries.bytes,
            Err(poisoned) => poisoned.into_inner().bytes,
        }
    }

    /// Look up a cached response. Returns `None` if not found or expired.
    pub fn get(&self, actor_key: &str, idem_key: &str) -> Option<EntityResponse> {
        let now = sim_now();
        let entries = match self.entries.read() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let entry = entries.by_actor.get(actor_key)?.get(idem_key)?;
        if is_expired(entry, now) {
            return None;
        }

        Some(entry.response.clone())
    }

    /// Look up a cached response only after post-dispatch effects have run.
    ///
    /// HTTP/OData callers use this stricter lookup so a retry after a dropped
    /// actor reply re-enters dispatch and fires effects instead of short-
    /// circuiting at the protocol boundary.
    pub fn get_after_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
    ) -> Option<EntityResponse> {
        let now = sim_now();
        let entries = self.entries.read().unwrap(); // ci-ok: infallible lock
        let entry = entries.by_actor.get(actor_key)?.get(idem_key)?;
        if is_expired(entry, now) || !entry.effects_applied {
            return None;
        }

        Some(entry.response.clone())
    }

    /// Cache a response for a given actor and idempotency key.
    ///
    /// Keeps a copy without event parameters or the durable key set. When
    /// the cache is over budget, replies are evicted (see the module docs).
    pub fn put(&self, actor_key: &str, idem_key: &str, response: &EntityResponse) {
        self.put_with_effects_applied(actor_key, idem_key, response, false);
    }

    /// Cache a response whose post-dispatch effects are known to be complete.
    pub fn put_effects_applied(&self, actor_key: &str, idem_key: &str, response: &EntityResponse) {
        self.put_with_effects_applied(actor_key, idem_key, response, true);
    }

    /// Mark a cached response as having completed post-dispatch effects.
    pub fn mark_effects_applied(&self, actor_key: &str, idem_key: &str) -> bool {
        let now = sim_now();
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(entry) = entries
            .by_actor
            .get_mut(actor_key)
            .and_then(|actor_entries| actor_entries.get_mut(idem_key))
        else {
            return false;
        };
        if is_expired(entry, now) {
            return false;
        }

        entry.effects_applied = true;
        let seq = entry.seq;
        if let Some(keys) = entries.pending.remove(&seq) {
            entries.applied.insert(seq, keys);
        }
        true
    }

    fn put_with_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        response: &EntityResponse,
        effects_applied: bool,
    ) {
        let response = replay_copy(response);
        let bytes = estimated_entry_bytes(actor_key, idem_key, &response);

        let now = sim_now();
        let mut entries = self.entries.write().unwrap(); // ci-ok: infallible lock
        entries.remove(actor_key, idem_key);
        entries.evict_expired(now);

        // A reply larger than the whole budget is not cached. The durable key
        // set still deduplicates its retry, but as after a restart, a retry
        // of a dropped reply then runs without the reply's spawns, timers and
        // (for a non-composite action) triggers.
        if bytes > self.budget_bytes {
            return;
        }

        let seq = entries.next_seq;
        entries.next_seq += 1;
        let order = if effects_applied {
            &mut entries.applied
        } else {
            &mut entries.pending
        };
        order.insert(seq, (actor_key.to_string(), idem_key.to_string()));
        entries
            .by_actor
            .entry(actor_key.to_string())
            .or_default()
            .insert(
                idem_key.to_string(),
                IdempotencyEntry {
                    response,
                    created_at: now,
                    effects_applied,
                    bytes,
                    seq,
                },
            );
        entries.bytes += bytes;

        while entries.bytes > self.budget_bytes {
            if !entries.evict_one() {
                break;
            }
        }
        debug_assert!(entries.bytes <= self.budget_bytes);
        debug_assert_eq!(
            entries.len(),
            entries.by_actor.values().map(BTreeMap::len).sum::<usize>()
        );
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether the actor's live state is still exactly the state a cached reply
/// was taken from, so a retry can be answered with it in full. The sequence
/// number alone cannot tell: it stays 0 without a journal, and a state
/// restored from a snapshot keeps it with fewer recent events. So the newest
/// live event must be this retry's, the event counts must match, and the live
/// history must be at least as long as the cached one.
pub(crate) fn live_state_is_the_reply(live: &EntityState, cached: &EntityState, key: &str) -> bool {
    live.events
        .back()
        .and_then(|event| event.idempotency_key.as_deref())
        == Some(key)
        && live.total_event_count == cached.total_event_count
        && live.events.len() >= cached.events.len()
}

/// The part of a reply a replay needs. Post-dispatch effects read event
/// metadata and the status clock (state clocks, source status); event parameters and the durable
/// key set dominate a busy entity's state and are not copied. A trigger fired
/// from a replayed reply that reads `events[].params` sees null there (the
/// same values are in the entity's fields).
fn replay_copy(response: &EntityResponse) -> EntityResponse {
    let state = &response.state;
    let events = state
        .events
        .iter()
        .map(|event| EntityEvent {
            action: event.action.clone(),
            from_status: event.from_status.clone(),
            to_status: event.to_status.clone(),
            timestamp: event.timestamp,
            params: serde_json::Value::Null,
            idempotency_key: event.idempotency_key.clone(),
            kind: temper_runtime::persistence::EventKind::Transition,
        })
        .collect();
    EntityResponse {
        success: response.success,
        refusal: response.refusal.clone(),
        state: EntityState {
            entity_type: state.entity_type.clone(),
            entity_id: state.entity_id.clone(),
            status: state.status.clone(),
            item_count: state.item_count,
            counters: state.counters.clone(),
            booleans: state.booleans.clone(),
            lists: state.lists.clone(),
            fields: state.fields.clone(),
            events,
            total_event_count: state.total_event_count,
            events_since_snapshot: state.events_since_snapshot,
            last_snapshot_sequence_nr: state.last_snapshot_sequence_nr,
            sequence_nr: state.sequence_nr,
            processed_idempotency_keys: BTreeMap::new(),
            status_clock: state.status_clock.clone(),
        },
        error: response.error.clone(),
        custom_effects: response.custom_effects.clone(),
        scheduled_actions: response.scheduled_actions.clone(),
        spawn_requests: response.spawn_requests.clone(),
        spec_governed: response.spec_governed,
        refused: response.refused,
    }
}

/// Rough heap bytes one cached reply holds: everything [`replay_copy`] keeps,
/// and the keys that index it (held by both the entry map and its order).
fn estimated_entry_bytes(actor_key: &str, idem_key: &str, response: &EntityResponse) -> usize {
    let state = &response.state;
    let named = |name: &String| VALUE_OVERHEAD_BYTES + name.len();
    let events: usize = state
        .events
        .iter()
        .map(|event| {
            VALUE_OVERHEAD_BYTES
                + event.action.len()
                + event.from_status.len()
                + event.to_status.len()
                + event.idempotency_key.as_ref().map_or(0, String::len)
        })
        .sum();
    let spawns: usize = response
        .spawn_requests
        .iter()
        .map(|spawn| {
            VALUE_OVERHEAD_BYTES
                + spawn.entity_type.len()
                + spawn.entity_id.len()
                + spawn
                    .copied_fields
                    .iter()
                    .map(|(name, value)| name.len() + json_bytes(value))
                    .sum::<usize>()
        })
        .sum();
    let lists: usize = state
        .lists
        .iter()
        .map(|(name, items)| named(name) + items.iter().map(named).sum::<usize>())
        .sum();
    let variables: usize = state.counters.keys().map(named).sum::<usize>()
        + state.booleans.keys().map(named).sum::<usize>();
    let effects: usize = response.custom_effects.iter().map(named).sum::<usize>()
        + response
            .scheduled_actions
            .iter()
            .map(|scheduled| named(&scheduled.action))
            .sum::<usize>();
    ENTRY_OVERHEAD_BYTES
        + 2 * (actor_key.len() + idem_key.len())
        + state.entity_type.len()
        + state.entity_id.len()
        + state.status.len()
        + response.error.as_ref().map_or(0, String::len)
        + json_bytes(&state.fields)
        + events
        + spawns
        + lists
        + variables
        + effects
}

/// Text bytes plus a per-value overhead, walked without recursion.
fn json_bytes(value: &serde_json::Value) -> usize {
    let mut total = 0;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        total += VALUE_OVERHEAD_BYTES;
        match value {
            serde_json::Value::String(text) => total += text.len(),
            serde_json::Value::Array(items) => pending.extend(items),
            serde_json::Value::Object(map) => {
                for (name, item) in map {
                    total += name.len();
                    pending.push(item);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
    total
}

#[cfg(test)]
mod tests;
