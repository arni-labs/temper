//! Idempotency cache for deduplicating agent retries.
//!
//! Recent `Idempotency-Key` → `EntityResponse` replies, kept per entity actor.
//! Entries expire after `IDEMPOTENCY_TTL_SECS`.
//!
//! Every dispatched action carries a key, so every write adds a reply. The
//! cache is therefore bounded by the bytes it holds across all actors
//! ([`IDEMPOTENCY_CACHE_BYTES_BUDGET`]), oldest reply evicted first, and a
//! cached reply keeps only what a replay reads: its recent events without their
//! parameters and without the actor's durable key set. An evicted reply is
//! still deduplicated by the actor's durable `processed_idempotency_keys`; only
//! the exact earlier reply is lost.

use std::collections::BTreeMap;
use std::sync::RwLock;

use temper_runtime::scheduler::sim_now;

use crate::entity_actor::EntityResponse;

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

#[derive(Default)]
struct Entries {
    /// actor_key → (idempotency_key → entry).
    by_actor: BTreeMap<String, BTreeMap<String, IdempotencyEntry>>,
    /// Insertion order across all actors, oldest first: seq → (actor_key, idempotency_key).
    order: BTreeMap<u64, (String, String)>,
    next_seq: u64,
    bytes: usize,
}

impl Entries {
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
        self.order.remove(&entry.seq);
        self.bytes -= entry.bytes;
    }

    fn remove_oldest(&mut self) -> bool {
        let Some((_, (actor_key, idem_key))) = self.order.pop_first() else {
            return false;
        };
        self.remove(&actor_key, &idem_key);
        true
    }

    fn oldest_created_at(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        let (_, (actor_key, idem_key)) = self.order.first_key_value()?;
        Some(self.by_actor.get(actor_key)?.get(idem_key)?.created_at)
    }
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

        // Check TTL
        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS {
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

        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS || !entry.effects_applied {
            return None;
        }

        Some(entry.response.clone())
    }

    /// Cache a response for a given actor and idempotency key.
    ///
    /// When the cache is over budget, the oldest replies are evicted.
    pub fn put(&self, actor_key: &str, idem_key: &str, response: EntityResponse) {
        self.put_with_effects_applied(actor_key, idem_key, response, false);
    }

    /// Cache a response whose post-dispatch effects are known to be complete.
    pub fn put_effects_applied(&self, actor_key: &str, idem_key: &str, response: EntityResponse) {
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

        let age = now.signed_duration_since(entry.created_at);
        if age.num_seconds() > IDEMPOTENCY_TTL_SECS {
            return false;
        }

        entry.effects_applied = true;
        true
    }

    fn put_with_effects_applied(
        &self,
        actor_key: &str,
        idem_key: &str,
        mut response: EntityResponse,
        effects_applied: bool,
    ) {
        // A replay reads event metadata (state clocks, source status), never
        // event parameters or the durable key set, which dominate a busy
        // entity's state.
        for event in response.state.events.iter_mut() {
            event.params = serde_json::Value::Null;
        }
        response.state.processed_idempotency_keys.clear();
        let bytes = estimated_entry_bytes(actor_key, idem_key, &response);

        let now = sim_now();
        let mut entries = self.entries.write().unwrap(); // ci-ok: infallible lock
        entries.remove(actor_key, idem_key);

        // Evict expired entries first: the oldest entry is the first to expire.
        while entries.oldest_created_at().is_some_and(|created_at| {
            now.signed_duration_since(created_at).num_seconds() > IDEMPOTENCY_TTL_SECS
        }) {
            entries.remove_oldest();
        }

        // A reply larger than the whole budget is not cached; the durable
        // key set still deduplicates it.
        if bytes > self.budget_bytes {
            return;
        }

        let seq = entries.next_seq;
        entries.next_seq += 1;
        entries
            .order
            .insert(seq, (actor_key.to_string(), idem_key.to_string()));
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
            if !entries.remove_oldest() {
                break;
            }
        }
        debug_assert!(entries.bytes <= self.budget_bytes);
        debug_assert_eq!(
            entries.order.len(),
            entries.by_actor.values().map(BTreeMap::len).sum::<usize>()
        );
    }
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Rough heap bytes one cached reply holds: its field values, its recent event
/// metadata, its spawn copies and the keys that index it.
fn estimated_entry_bytes(actor_key: &str, idem_key: &str, response: &EntityResponse) -> usize {
    let state = &response.state;
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
        .values()
        .flatten()
        .map(|item| VALUE_OVERHEAD_BYTES + item.len())
        .sum();
    ENTRY_OVERHEAD_BYTES
        + actor_key.len()
        + idem_key.len()
        + json_bytes(&state.fields)
        + events
        + spawns
        + lists
        + response
            .custom_effects
            .iter()
            .map(|effect| VALUE_OVERHEAD_BYTES + effect.len())
            .sum::<usize>()
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
