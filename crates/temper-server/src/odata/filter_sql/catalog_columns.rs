//! Comparisons on values a catalog row holds itself, not the field index:
//! the `status` and `entity_id` columns, and the record times in its state.
//!
//! SQL compares these by the same value and order a server read compares
//! them, so a predicate on them narrows the page exactly and paging's keyset
//! (`entity_id`, `$orderby` on a record time) runs as an SQL range instead of
//! a rescan from the first row.

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::persistence::catalog_state_time_sql;

use super::{TranslateCtx, odata_value_to_text};

/// The catalog column a property names, if any.
pub(super) fn catalog_column(field_name: &str) -> Option<&'static str> {
    match field_name {
        "Status" | "status" => Some("status"),
        "entity_id" => Some("entity_id"),
        _ => None,
    }
}

/// Whether `field op value` (`eq` or `ne`) on a catalog value needs no
/// recheck. A column takes a string or null; a record time takes an RFC 3339
/// string, a date-time or null. Other kernel event metadata has no SQL form.
pub(super) fn catalog_comparison_is_lossless(field_name: &str, value: &ODataValue) -> bool {
    if catalog_column(field_name).is_some() {
        return matches!(value, ODataValue::Null | ODataValue::String(_));
    }
    catalog_state_time_sql(field_name).is_some()
        && match value {
            ODataValue::Null | ODataValue::DateTimeOffset(_) => true,
            // Text that is not a time compares as text in memory, not as
            // the instant the SQL compares.
            ODataValue::String(text) => chrono::DateTime::parse_from_rfc3339(text).is_ok(),
            _ => false,
        }
}

/// Whether `property op literal` (`gt`, `ge`, `lt`, `le`) on a catalog value
/// needs no recheck. The property must be on the left: the translation keeps
/// the operator as written.
pub(super) fn catalog_ordering_is_lossless(left: &FilterExpr, right: &FilterExpr) -> bool {
    match (left, right) {
        (FilterExpr::Property(field_name), FilterExpr::Literal(value)) => {
            !matches!(value, ODataValue::Null) && catalog_comparison_is_lossless(field_name, value)
        }
        _ => false,
    }
}

/// Translate a comparison on a catalog column or record time.
pub(super) fn translate_catalog_comparison(
    field_name: &str,
    sql_op: &str,
    op: BinaryOperator,
    value: &ODataValue,
    ctx: &mut TranslateCtx,
) -> Option<String> {
    let (expr, text_value) = if let Some(column) = catalog_column(field_name) {
        if matches!(value, ODataValue::Null) {
            return null_comparison(column, op);
        }
        (column.to_string(), odata_value_to_text(value)?)
    } else {
        // The stored time through `catalog_state_time_sql`, the literal as a
        // server read normalizes it: both nine-digit UTC text.
        let time = catalog_state_time_sql(field_name)?;
        let literal = match value {
            ODataValue::Null => return null_comparison(&time, op),
            ODataValue::String(text) => serde_json::Value::String(text.clone()),
            ODataValue::DateTimeOffset(at) => serde_json::Value::String(at.to_rfc3339()),
            _ => return None,
        };
        let text_value = crate::query_eval::normalize_event_time(literal)
            .as_str()?
            .to_string();
        (time, text_value)
    };
    let value_param = ctx.add_param(text_value);
    Some(format!("{expr} {sql_op} {value_param}"))
}

fn null_comparison(expr: &str, op: BinaryOperator) -> Option<String> {
    match op {
        BinaryOperator::Eq => Some(format!("{expr} IS NULL")),
        BinaryOperator::Ne => Some(format!("{expr} IS NOT NULL")),
        _ => None,
    }
}

#[cfg(test)]
#[path = "catalog_columns_test.rs"]
mod tests;
