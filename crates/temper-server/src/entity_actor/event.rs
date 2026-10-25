//! The journal's record of one transition, and what its effects wrote.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
    /// Which kind of event the kernel wrote. Only the kernel's delete writes a
    /// tombstone, whatever a spec names its actions.
    #[serde(default)]
    pub kind: temper_runtime::persistence::EventKind,
    /// What the transition's effects wrote, recorded when it ran. Replay
    /// restores it instead of running the effects again. `None` on events
    /// that run no effects (creation, field updates, tombstones) and on
    /// transitions journaled before outcomes were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<EffectOutcome>,
}

/// What a transition's effects wrote into the entity: every counter, boolean,
/// list and field whose value they changed, with its value afterwards.
///
/// Some effects produce their value only when they run (a spawn mints its
/// child's id), and every effect computes with the spec in force at that
/// moment. So the event records the result and replay restores it: replay
/// never mints, spawns, schedules or computes again. The record is the
/// difference between the state before and after the effects, so no effect
/// can write something the record leaves out. Effects only set values; none
/// removes a variable or a field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectOutcome {
    /// Counters the effects changed, with their new values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub counters: BTreeMap<String, usize>,
    /// Booleans the effects changed, with their new values.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub booleans: BTreeMap<String, bool>,
    /// Lists the effects changed, with their new contents.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lists: BTreeMap<String, Vec<String>>,
    /// Fields the effects wrote directly (a spawn's stored child id), with
    /// their new values. Action params reach fields from the event's params.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub fields: serde_json::Map<String, serde_json::Value>,
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
            kind: temper_runtime::persistence::EventKind::Transition,
            outcome: None,
        }
    }
}
