//! Event times for catalog rows written before the kernel kept them
//! (ADR-0187, "Backfill").
//!
//! A record made before 31 Oct 2026 and not moved since has no
//! `last_event_at` in its catalog state, and its `first_event_at` column is
//! empty when the creation-time migration found no time for it. An event-time
//! page orders such a row by the missing-time sentinel, so it comes first in a
//! newest-first list. Each store walks its catalog once per start and, for each
//! row missing a time, reads what the record's own journal says:
//!
//! - `first_event_at` is the time of the creation event, journal sequence one,
//!   as the creation-time migration and the actor read it. A record whose
//!   creation is no longer held keeps an unknown creation time
//!   ([`FirstFill::CreationNotHeld`]): a later event's time is not when the
//!   record was made.
//! - `last_event_at` is the time of the newest event. A record whose newest
//!   event is covered by a snapshot is left as it is
//!   ([`LastFill::HeldBySnapshot`]): its snapshot predates the time, the
//!   actor recovers from that snapshot with no time, and the boot projection
//!   refresh would write that back over a filled row.
//!
//! [`EventTimeBackfillMode`] says whether a start only counts (the default)
//! or also fills.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

/// The environment variable that switches a start from counting to filling.
pub const EVENT_TIME_BACKFILL_ENV: &str = "TEMPER_EVENT_TIME_BACKFILL";

/// What a start does with catalog rows missing an event time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventTimeBackfillMode {
    /// Count the rows and what their journals could fill, and write nothing.
    Count,
    /// Fill each row its journal can, once per store, behind a durable cursor.
    Fill,
}

impl EventTimeBackfillMode {
    /// [`EVENT_TIME_BACKFILL_ENV`], read once: `fill`, or else count.
    pub fn from_env() -> Self {
        static MODE: OnceLock<EventTimeBackfillMode> = OnceLock::new();
        *MODE.get_or_init(|| {
            Self::parse(
                std::env::var(EVENT_TIME_BACKFILL_ENV) // determinism-ok: read once at startup
                    .ok()
                    .as_deref(),
            )
        })
    }

    /// `fill` (any case, trimmed) fills; any other value or none counts.
    pub fn parse(value: Option<&str>) -> Self {
        match value
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("fill") => Self::Fill,
            _ => Self::Count,
        }
    }

    /// The mode's name in logs and on the observe route.
    pub fn label(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Fill => "fill",
        }
    }
}

/// What a record's journal says about the times its catalog row is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalTimes {
    /// Whether the catalog row has no `first_event_at`.
    pub first_missing: bool,
    /// Whether the catalog row's state has no `last_event_at`.
    pub last_missing: bool,
    /// Whether the catalog row's state is a JSON object a time can be set in.
    pub state_is_object: bool,
    /// Whether the journal holds any event for the record.
    pub has_events: bool,
    /// The creation event's (sequence one) time, as stored.
    pub creation_at: Option<String>,
    /// The newest event's sequence.
    pub newest_sequence_nr: Option<u64>,
    /// The newest event's time, as stored.
    pub newest_at: Option<String>,
    /// The sequence of the record's snapshot, if it has one.
    pub snapshot_sequence_nr: Option<u64>,
}

/// What happens to a row's `first_event_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstFill {
    /// The row has one.
    Present,
    /// The creation event's time.
    Fill(DateTime<Utc>),
    /// The journal holds later events but not the creation: left unknown.
    CreationNotHeld,
    /// The journal has no usable time for it.
    Unfillable,
}

/// What happens to a row's `last_event_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastFill {
    /// The row has one.
    Present,
    /// The newest event's time.
    Fill(DateTime<Utc>),
    /// The newest event is covered by a snapshot that has no time: left as is.
    HeldBySnapshot,
    /// The journal has no usable time for it, or the state is not an object.
    Unfillable,
}

impl JournalTimes {
    /// What the backfill does with this row's two times.
    pub fn plan(&self) -> (FirstFill, LastFill) {
        let first = if !self.first_missing {
            FirstFill::Present
        } else if let Some(at) = self.creation_at.as_deref().and_then(journal_time) {
            FirstFill::Fill(at)
        } else if self.creation_at.is_none() && self.has_events {
            FirstFill::CreationNotHeld
        } else {
            FirstFill::Unfillable
        };
        let last = if !self.last_missing {
            LastFill::Present
        } else if !self.state_is_object {
            LastFill::Unfillable
        } else {
            match (
                self.newest_sequence_nr,
                self.newest_at.as_deref().and_then(journal_time),
            ) {
                (Some(newest), Some(_))
                    if self
                        .snapshot_sequence_nr
                        .is_some_and(|snapshot| snapshot >= newest) =>
                {
                    LastFill::HeldBySnapshot
                }
                (Some(_), Some(at)) => LastFill::Fill(at),
                _ => LastFill::Unfillable,
            }
        };
        (first, last)
    }
}

/// A stored journal time as an instant: RFC 3339 (an event's own timestamp)
/// or the SQL `datetime('now')` text a journal row's `created_at` holds, in
/// UTC. `None` when it is neither.
pub fn journal_time(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    DateTime::parse_from_rfc3339(text)
        .map(|at| at.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
                .ok()
                .map(|at| at.and_utc())
        })
}

/// `first_event_at` as the creation-time migration and the actor store it.
pub fn first_event_at_text(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// `last_event_at` as the actor's projection stores it (`LastEvent`).
pub fn last_event_at_value(at: DateTime<Utc>) -> serde_json::Value {
    serde_json::to_value(at).unwrap_or(serde_json::Value::Null)
}

/// What one store's fill has done so far, as its cursor row keeps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTimeFill {
    /// Catalog rows visited.
    pub records: u64,
    /// Rows given a `first_event_at`.
    pub first_filled: u64,
    /// Rows given a `last_event_at`.
    pub last_filled: u64,
    /// Catalog rows written: one per row that changed, both times at once.
    pub rows_written: u64,
}

impl EventTimeFill {
    /// Count one visited row, whose write as `plan` changed `written` rows.
    pub fn add(&mut self, plan: (FirstFill, LastFill), written: u64) {
        self.records += 1;
        self.rows_written += written;
        if written > 0 {
            self.first_filled += u64::from(matches!(plan.0, FirstFill::Fill(_)));
            self.last_filled += u64::from(matches!(plan.1, LastFill::Fill(_)));
        }
    }

    /// Log a finished fill (WARN: see [`log_event_time_gaps`]).
    pub fn log_complete(&self, elapsed_ms: u64) {
        tracing::warn!(
            records = self.records,
            first_filled = self.first_filled,
            last_filled = self.last_filled,
            rows_written = self.rows_written,
            elapsed_ms,
            "event time backfill: fill complete"
        );
    }
}

/// The rows of one entity type missing an event time, and what their journals
/// can fill. `missing_first = first_fillable + first_creation_not_held +
/// first_unfillable`, and likewise for the last time.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTimeGaps {
    /// The tenant.
    pub tenant: String,
    /// The entity type.
    pub entity_type: String,
    /// Catalog rows looked at.
    pub records: u64,
    /// Rows with no `first_event_at`.
    pub missing_first: u64,
    /// Of those, rows whose creation event has a time: the fill sets them.
    pub first_fillable: u64,
    /// Of those, rows whose creation event is no longer held: left unknown.
    pub first_creation_not_held: u64,
    /// Of those, rows with no usable journal time.
    pub first_unfillable: u64,
    /// Rows with no `last_event_at`.
    pub missing_last: u64,
    /// Of those, rows whose newest event has a time: the fill sets them.
    pub last_fillable: u64,
    /// Of those, rows whose newest event is covered by a snapshot: left as is.
    pub last_held_by_snapshot: u64,
    /// Of those, rows with no usable journal time or no state object.
    pub last_unfillable: u64,
}

impl EventTimeGaps {
    fn add(&mut self, first: FirstFill, last: LastFill) {
        self.records += 1;
        match first {
            FirstFill::Present => {}
            FirstFill::Fill(_) => self.first_fillable += 1,
            FirstFill::CreationNotHeld => self.first_creation_not_held += 1,
            FirstFill::Unfillable => self.first_unfillable += 1,
        }
        self.missing_first += u64::from(first != FirstFill::Present);
        match last {
            LastFill::Present => {}
            LastFill::Fill(_) => self.last_fillable += 1,
            LastFill::HeldBySnapshot => self.last_held_by_snapshot += 1,
            LastFill::Unfillable => self.last_unfillable += 1,
        }
        self.missing_last += u64::from(last != LastFill::Present);
    }

    fn absorb(&mut self, other: &Self) {
        self.records += other.records;
        self.missing_first += other.missing_first;
        self.first_fillable += other.first_fillable;
        self.first_creation_not_held += other.first_creation_not_held;
        self.first_unfillable += other.first_unfillable;
        self.missing_last += other.missing_last;
        self.last_fillable += other.last_fillable;
        self.last_held_by_snapshot += other.last_held_by_snapshot;
        self.last_unfillable += other.last_unfillable;
    }

    fn has_gaps(&self) -> bool {
        self.missing_first + self.missing_last > 0
    }
}

/// Counts by tenant and entity type, in key order.
#[derive(Debug, Clone, Default)]
pub struct EventTimeTally(BTreeMap<(String, String), EventTimeGaps>);

impl EventTimeTally {
    /// Count one catalog row of `tenant`'s `entity_type`.
    pub fn record(&mut self, tenant: &str, entity_type: &str, first: FirstFill, last: LastFill) {
        self.0
            .entry((tenant.to_string(), entity_type.to_string()))
            .or_insert_with(|| EventTimeGaps {
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                ..EventTimeGaps::default()
            })
            .add(first, last);
    }

    /// Every type's counts, by tenant and type.
    pub fn into_gaps(self) -> Vec<EventTimeGaps> {
        self.0.into_values().collect()
    }
}

/// Log a store's counts: one line per tenant with gaps, naming each type's
/// counts, and one line for the store. WARN, because deployed kernels export
/// only WARN and above to Datadog, and an operator reads these there. A
/// counted store with no gaps logs its line at INFO, so a healthy store adds
/// nothing to Datadog on each start; after a fill it is always WARN.
pub fn log_event_time_gaps(mode: EventTimeBackfillMode, gaps: &[EventTimeGaps], elapsed_ms: u64) {
    let mut by_tenant: BTreeMap<&str, Vec<&EventTimeGaps>> = BTreeMap::new();
    for gap in gaps {
        by_tenant.entry(gap.tenant.as_str()).or_default().push(gap);
    }
    let mut store = EventTimeGaps::default();
    for (tenant, types) in &by_tenant {
        let mut total = EventTimeGaps::default();
        for gap in types {
            total.absorb(gap);
        }
        store.absorb(&total);
        if !total.has_gaps() {
            continue;
        }
        let by_type = serde_json::to_string(
            &types
                .iter()
                .filter(|gap| gap.has_gaps())
                .copied()
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        tracing::warn!(
            mode = mode.label(),
            tenant = *tenant,
            records = total.records,
            missing_first = total.missing_first,
            first_fillable = total.first_fillable,
            first_creation_not_held = total.first_creation_not_held,
            first_unfillable = total.first_unfillable,
            missing_last = total.missing_last,
            last_fillable = total.last_fillable,
            last_held_by_snapshot = total.last_held_by_snapshot,
            last_unfillable = total.last_unfillable,
            by_type = %by_type,
            "event time backfill: tenant records missing an event time"
        );
    }
    macro_rules! store_line {
        ($level:ident) => {
            tracing::$level!(
                mode = mode.label(),
                tenants = by_tenant.len() as u64,
                records = store.records,
                missing_first = store.missing_first,
                first_fillable = store.first_fillable,
                missing_last = store.missing_last,
                last_fillable = store.last_fillable,
                elapsed_ms,
                "event time backfill: store counted"
            )
        };
    }
    if store.has_gaps() || mode == EventTimeBackfillMode::Fill {
        store_line!(warn);
    } else {
        store_line!(info);
    }
}

#[cfg(test)]
#[path = "event_time_backfill_test.rs"]
mod tests;
