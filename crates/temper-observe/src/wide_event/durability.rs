use std::collections::BTreeMap;

use crate::wide_event::{EventKind, WideEvent, event_timestamp, new_span_id};

/// Input for the wide event of a store that stops or starts confirming moves
/// because its backup fell behind or caught up.
pub struct DurabilityChangeInput<'a> {
    /// The store's database.
    pub database: &'a str,
    /// What guards the moves (e.g. "push_budget").
    pub guard: &'a str,
    /// Whether the store confirms moves from now on.
    pub confirming: bool,
    /// Why, in words.
    pub why: &'a str,
    /// Confirmed moves the backup lacks.
    pub unpushed_moves: u64,
    /// How long the oldest of them has waited, in milliseconds.
    pub oldest_wait_ms: u64,
    /// How long one may wait before moves are refused, in milliseconds.
    pub budget_ms: u64,
}

/// Build a WideEvent from a store's change between confirming and refusing
/// moves. Refusing is reported as a failure, with `why` as its message.
pub fn from_durability_change(input: DurabilityChangeInput<'_>) -> WideEvent {
    let (from, to) = if input.confirming {
        ("refusing", "confirming")
    } else {
        ("confirming", "refusing")
    };
    let mut tags = BTreeMap::new();
    tags.insert("guard".into(), input.guard.into());
    tags.insert("to_status".into(), to.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("database".into(), serde_json::json!(input.database));
    attributes.insert("why".into(), serde_json::json!(input.why));
    if !input.confirming {
        attributes.insert("error.message".into(), serde_json::json!(input.why));
        attributes.insert("error.type".into(), serde_json::json!("not_durable"));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert("unpushed_moves".into(), input.unpushed_moves as f64);
    measurements.insert("oldest_wait_ms".into(), input.oldest_wait_ms as f64);
    measurements.insert("budget_ms".into(), input.budget_ms as f64);

    WideEvent {
        event_kind: EventKind::Durability,
        entity_type: "store".into(),
        entity_id: input.database.into(),
        operation: input.guard.into(),
        from_status: from.into(),
        to_status: to.into(),
        success: input.confirming,
        duration_ns: 0,
        timestamp: event_timestamp(),
        trace_id: String::new(),
        span_id: new_span_id(),
        tags,
        attributes,
        measurements,
    }
}
