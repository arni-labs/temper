//! Types for the entity actor: messages, state, events, and responses.

use std::collections::{BTreeMap, VecDeque};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use temper_runtime::actor::Message;

// Runtime resource budgets. Domain counter constraints belong to the spec.

/// Maximum unsnapshotted events an actor may replay/hot-hold before refusing new transitions.
pub const MAX_EVENTS_SINCE_SNAPSHOT: usize = 10_000;
/// Backward-compatible alias for older callers; budget enforcement is tail-based.
pub const MAX_EVENTS_PER_ENTITY: usize = MAX_EVENTS_SINCE_SNAPSHOT;
/// Default number of recent events retained in memory per entity.
pub const RECENT_EVENTS_BUDGET_DEFAULT: usize = 50;
/// Maximum durable idempotency keys retained per entity.
pub const MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY: usize = 1_000;

/// Number of recent events retained in memory per entity.
///
/// Controlled by `TEMPER_RECENT_EVENTS_BUDGET` (default 50).
pub fn recent_events_budget() -> usize {
    static RECENT_EVENTS_BUDGET: OnceLock<usize> = OnceLock::new();
    *RECENT_EVENTS_BUDGET.get_or_init(|| {
        std::env::var("TEMPER_RECENT_EVENTS_BUDGET") // determinism-ok: read once at startup
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(RECENT_EVENTS_BUDGET_DEFAULT)
    })
}

/// Messages the entity actor can receive.
#[derive(Debug)]
pub enum EntityMsg {
    /// Execute a state machine action (e.g., "SubmitOrder", "CancelOrder").
    Action {
        name: String,
        params: serde_json::Value,
        /// Related entities' statuses the action's guards read (resolved by the dispatch layer).
        related: temper_jit::table::RelatedMap,
        /// ADR-0048 sub-decision 5: idempotency key threaded through so the
        /// actor can dedupe against `IdempotencyCache` before executing.
        /// Covers the race where a dispatch-layer retry produces a second
        /// in-flight ask after the first one already processed.
        idempotency_key: Option<String>,
        /// Digest of the exact local state used for an external Cedar
        /// decision. Internal dispatches omit it.
        expected_authorization_precondition: Option<String>,
        /// Set when the runtime makes this move itself (a state timer):
        /// recorded on the event as its actor and cause.
        origin: Option<super::MoveOrigin>,
    },
    /// Get the current entity state.
    GetState,
    /// Get a specific field value.
    GetField { field: String },
    /// Update entity fields (PATCH: merge, PUT: replace).
    UpdateFields {
        fields: serde_json::Value,
        replace: bool,
        /// Digest of the exact state used for an external authorization
        /// decision. The actor rejects the update if that state changed before
        /// this message reached its mailbox.
        expected_precondition: Option<String>,
    },
    /// Delete this entity.
    Delete {
        /// Digest of the exact local state used for an external Cedar
        /// decision. Internal dispatches omit it.
        expected_authorization_precondition: Option<String>,
    },
}

impl Message for EntityMsg {}

/// The entity's runtime state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityState {
    /// Entity type (e.g., "Order").
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Current status (state machine state).
    pub status: String,
    /// Item count (legacy — prefer `counters["items"]` for new code).
    pub item_count: usize,
    /// Named counter variables (e.g., "items", "review_cycles").
    #[serde(default)]
    pub counters: BTreeMap<String, usize>,
    /// Named boolean variables (e.g., "assignee_set", "has_address").
    #[serde(default)]
    pub booleans: BTreeMap<String, bool>,
    /// Named list variables (e.g., "tags", "approvers").
    #[serde(default)]
    pub lists: BTreeMap<String, Vec<String>>,
    /// All entity fields as a JSON object.
    pub fields: serde_json::Value,
    /// Recent event log (bounded in-memory history for observability).
    #[serde(default)]
    pub events: VecDeque<EntityEvent>,
    /// Total event count ever applied to this entity.
    #[serde(default)]
    pub total_event_count: usize,
    /// Number of events applied after the latest durable snapshot boundary.
    #[serde(default)]
    pub events_since_snapshot: usize,
    /// Sequence number captured by the latest durable snapshot.
    #[serde(default)]
    pub last_snapshot_sequence_nr: u64,
    /// Current event sourcing sequence number (for persistence).
    #[serde(default)]
    pub sequence_nr: u64,
    /// Idempotency keys that have already produced durable events.
    ///
    /// Rebuilt from [`EntityEvent::idempotency_key`] during replay so a retry
    /// after process restart can return success without re-applying the action.
    #[serde(default)]
    pub processed_idempotency_keys: BTreeMap<String, u64>,
    /// When the current status began, and when each action last ran since.
    /// Kept in snapshots, which leave `events` out.
    #[serde(default)]
    pub status_clock: StatusClock,
}

/// When an entity's current status began, and when each action was last
/// applied since then: how long a state timer's clock has been running. A state
/// restored from a snapshot has no `events` before the snapshot to read this
/// from, so it is kept here instead (ADR-0056).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatusClock {
    /// When the event that entered the current status was applied.
    #[serde(default)]
    pub entered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The last time each action was applied since the status was entered.
    #[serde(default)]
    pub actions: BTreeMap<String, chrono::DateTime<chrono::Utc>>,
}

impl StatusClock {
    fn record(&mut self, event: &EntityEvent) {
        if event.from_status != event.to_status {
            self.entered_at = Some(event.timestamp);
            self.actions.clear();
        } else {
            self.actions.insert(event.action.clone(), event.timestamp);
        }
    }
}

impl EntityState {
    /// Return true if this entity can accept one more event under budget.
    pub fn can_accept_event(&self) -> bool {
        self.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT
    }

    /// Append an event to recent history while enforcing bounded memory.
    pub fn push_event_bounded(&mut self, event: EntityEvent) {
        self.total_event_count = self.total_event_count.saturating_add(1);
        self.events_since_snapshot = self.events_since_snapshot.saturating_add(1);
        if let Some(key) = event.idempotency_key.as_deref() {
            self.record_processed_idempotency_key(key);
        }
        self.status_clock.record(&event);
        self.events.push_back(event);

        let budget = recent_events_budget();
        while self.events.len() > budget {
            self.events.pop_front();
        }
    }

    /// Where this state is in the record's history. With a journal that is the
    /// journal sequence; without one the sequence stays put, and the count of
    /// events applied moves instead. Both only grow while the record lives.
    pub fn event_sequence(&self) -> u64 {
        self.sequence_nr.max(self.total_event_count as u64)
    }

    pub fn has_processed_idempotency_key(&self, key: &str) -> bool {
        self.processed_idempotency_keys.contains_key(key)
    }

    fn record_processed_idempotency_key(&mut self, key: &str) {
        let sequence = self.event_sequence();
        self.processed_idempotency_keys
            .insert(key.to_string(), sequence);

        while self.processed_idempotency_keys.len() > MAX_DURABLE_IDEMPOTENCY_KEYS_PER_ENTITY {
            let Some(oldest_key) = self
                .processed_idempotency_keys
                .iter()
                .min_by_key(|(_, sequence)| **sequence)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.processed_idempotency_keys.remove(&oldest_key);
        }
    }
}

/// A recorded state transition event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityEvent {
    /// The action that triggered the transition.
    pub action: String,
    /// The status before the transition.
    pub from_status: String,
    /// The status after the transition.
    pub to_status: String,
    /// When the transition occurred.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Parameters passed with the action.
    pub params: serde_json::Value,
    /// Optional idempotency key that caused this transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Who made this move and what led to it, when the runtime made it
    /// itself: a state timer's move names [`super::STATE_TIMER_ACTOR`] and the
    /// move that armed it. Absent when the dispatch's caller made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<super::MoveOrigin>,
    /// Which kind of event the kernel wrote. Only the kernel's delete writes a
    /// tombstone, whatever a spec names its actions.
    #[serde(default)]
    pub kind: temper_runtime::persistence::EventKind,
}

impl EntityEvent {
    /// The `Created` event that starts a journal, carrying no params.
    pub(crate) fn bootstrap(to_status: String) -> Self {
        Self {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status,
            timestamp: temper_runtime::scheduler::sim_now(),
            params: serde_json::json!({}),
            idempotency_key: None,
            origin: None,
            kind: temper_runtime::persistence::EventKind::Transition,
        }
    }
}

/// Default value for `spec_governed`: actions are spec-governed unless explicitly marked otherwise.
fn default_spec_governed() -> bool {
    true
}
/// Serde skip predicate: skip serializing `spec_governed` when it is `true` (the default).
fn is_true(v: &bool) -> bool {
    *v
}

/// The response returned from an action or query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityResponse {
    /// Whether the action succeeded.
    pub success: bool,
    /// The current entity state after the action.
    pub state: EntityState,
    /// Error message if the action failed.
    pub error: Option<String>,
    /// Custom effects emitted during this transition (for hook dispatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_effects: Vec<String>,
    /// Scheduled actions to fire after delays (for timer dispatch).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scheduled_actions: Vec<crate::entity_actor::effects::ScheduledAction>,
    /// Spawn requests for child entities (executed by dispatch pipeline).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawn_requests: Vec<crate::entity_actor::effects::SpawnRequest>,
    /// Whether the action was governed by a state-machine spec. Defaults to `true`.
    #[serde(default = "default_spec_governed", skip_serializing_if = "is_true")]
    pub spec_governed: bool,
}

#[cfg(test)]
mod tests {
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
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
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
}
