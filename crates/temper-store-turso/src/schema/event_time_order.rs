//! The catalog's event-time sort keys and the indexes that read a page in
//! their order (ADR-0187).
//!
//! A key is the expression [`event_time_key_sql`] gives: the event time's
//! text (UTC, ending in `Z`, 0 to 9 fraction digits) as the fixed-width
//! `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn`, or `MISSING_EVENT_TIME_KEY` when the row
//! has none, the same key `temper_runtime::persistence::event_time_key`
//! gives in Rust. `last_event_at` is read from the row's `state`;
//! `first_event_at` from its column, which is what a catalog read shows
//! (`store/field_index.rs`, `load_entity_catalog_rows`).
//!
//! A query orders by the same expression text an index is built on, so the
//! planner matches the two. A page orders by the key and then `entity_id`
//! ascending, in either direction of the key, so each key has an index per
//! direction; `status` comes last so a status filter is checked from the
//! index, without reading the row.

use temper_runtime::persistence::{EventTimeColumn, MISSING_EVENT_TIME_KEY};

/// The SQL sort key of `column` on an `entity_catalog` row.
pub fn event_time_key_sql(column: EventTimeColumn) -> String {
    let text = match column {
        EventTimeColumn::First => "first_event_at",
        EventTimeColumn::Last => "json_extract(state, '$.last_event_at')",
    };
    format!(
        "COALESCE(substr({text}, 1, 19) || '.' || \
         substr(rtrim(substr({text}, 21), 'Z') || '000000000', 1, 9), \
         '{MISSING_EVENT_TIME_KEY}')"
    )
}

/// The index that reads `column`'s key in this direction, then `entity_id`
/// ascending.
pub fn event_time_index_name(column: EventTimeColumn, descending: bool) -> &'static str {
    match (column, descending) {
        (EventTimeColumn::First, false) => "idx_entity_catalog_first_event_asc",
        (EventTimeColumn::First, true) => "idx_entity_catalog_first_event_desc",
        (EventTimeColumn::Last, false) => "idx_entity_catalog_last_event_asc",
        (EventTimeColumn::Last, true) => "idx_entity_catalog_last_event_desc",
    }
}

pub(crate) fn create_event_time_index(column: EventTimeColumn, descending: bool) -> String {
    let name = event_time_index_name(column, descending);
    let key = event_time_key_sql(column);
    let direction = if descending { " DESC" } else { "" };
    format!(
        "CREATE INDEX IF NOT EXISTS {name} \
         ON entity_catalog(tenant, entity_type, {key}{direction}, entity_id, status)"
    )
}

/// Every event-time index, as `CREATE INDEX IF NOT EXISTS` statements.
pub fn create_event_time_indexes() -> Vec<String> {
    let mut statements = Vec::with_capacity(4);
    for column in [EventTimeColumn::Last, EventTimeColumn::First] {
        for descending in [true, false] {
            statements.push(create_event_time_index(column, descending));
        }
    }
    statements
}
