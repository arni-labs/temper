//! The keyset a continuation resumes in SQL (ADR-0187, ADR-0188).

use temper_odata::query::types::OrderByClause;
use temper_runtime::persistence::{
    CatalogOrderKey, FieldLiteral, KeysetValue, MISSING_EVENT_TIME_KEY, event_time_key,
};

use super::{Cursor, CursorKey, ID_PROPERTY, cursor_for_row};

/// The cursor as the keys the stores compare, one per clause of `order`, or
/// `None` when a clause is not a key the store can seek by. An event time is
/// its instant's key and an absent one sorts as `MISSING_EVENT_TIME_KEY`; an
/// id or a status is its text; an app field is its number or string, or
/// none (ADR-0188). `Id` and `id` are read from the record's body, which
/// need not hold its id, so they fold.
pub(super) fn sql_keyset(cursor: &Cursor, order: &[OrderByClause]) -> Option<Vec<KeysetValue>> {
    if cursor.keys.len() != order.len() {
        return None;
    }
    let text = |key: &CursorKey| {
        key.value
            .as_str()
            .filter(|_| key.present)
            .map(|text| KeysetValue::Key(text.to_string()))
    };
    order
        .iter()
        .zip(&cursor.keys)
        .map(
            |(clause, key)| match CatalogOrderKey::of(&clause.property) {
                CatalogOrderKey::EntityId if clause.property == ID_PROPERTY => text(key),
                CatalogOrderKey::Status => text(key),
                CatalogOrderKey::EventTime(_) if key.present => key
                    .value
                    .as_str()
                    .and_then(event_time_key)
                    .map(KeysetValue::Key),
                CatalogOrderKey::EventTime(_) => {
                    Some(KeysetValue::Key(MISSING_EVENT_TIME_KEY.to_string()))
                }
                CatalogOrderKey::Field(_) if !key.present => Some(KeysetValue::Field(None)),
                CatalogOrderKey::Field(_) => match &key.value {
                    serde_json::Value::String(text) => {
                        Some(KeysetValue::Field(Some(FieldLiteral::Text(text.clone()))))
                    }
                    serde_json::Value::Number(number) => Some(KeysetValue::Field(Some(
                        FieldLiteral::Number(number.to_string()),
                    ))),
                    _ => None,
                },
                CatalogOrderKey::EntityId => None,
            },
        )
        .collect()
}

/// The keys the stores compare for `row` under `order` ([`sql_keyset`]): where
/// a walk's next store page starts after it.
pub(in crate::odata::query_plane_read) fn sql_keyset_for_row(
    row: &serde_json::Value,
    order: &[OrderByClause],
) -> Option<Vec<KeysetValue>> {
    sql_keyset(&cursor_for_row(row, order), order)
}
