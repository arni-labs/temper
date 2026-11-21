//! How a catalog condition reads a row in Postgres's SQL (ADR-0188).
//!
//! `jsonb_typeof` names a field's JSON type (`number`, `string`, `boolean`,
//! `null`, `array`, `object`), or is NULL when `fields` has no such key. A
//! number is compared as `double precision`, the IEEE double the evaluator
//! holds, not as `numeric`, which tells apart decimals one double cannot. A
//! string is compared under `COLLATE "C"`, byte by byte as Rust compares
//! `str`, whatever the database's collation is. A value is read only under a
//! `CASE` on its type, because Postgres does not promise to test an `AND`'s
//! sides in order and a cast of a string to a number fails.

use temper_runtime::persistence::{CatalogSql, EventTimeColumn, FieldClass};

use crate::query_page::event_time_key_sql;

/// Postgres's [`CatalogSql`].
pub(crate) struct PostgresCatalogSql;

/// `value` as a Postgres string literal.
pub(crate) fn postgres_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn json_type(class: FieldClass) -> &'static str {
    match class {
        FieldClass::Number => "'number'",
        FieldClass::Text => "'string'",
        FieldClass::Boolean => "'boolean'",
    }
}

impl CatalogSql for PostgresCatalogSql {
    fn event_time_key(&self, column: EventTimeColumn) -> String {
        event_time_key_sql(column)
    }

    fn field_has_key(&self, field: &str) -> String {
        let key = postgres_string_literal(field);
        format!("((fields -> {key}) IS NOT NULL)")
    }

    fn field_present(&self, field: &str) -> String {
        let key = postgres_string_literal(field);
        format!("(COALESCE(jsonb_typeof(fields -> {key}), 'null') <> 'null')")
    }

    fn field_is(&self, field: &str, class: FieldClass) -> String {
        let key = postgres_string_literal(field);
        format!(
            "(COALESCE(jsonb_typeof(fields -> {key}), '') = {})",
            json_type(class)
        )
    }

    fn field_value(&self, field: &str, class: FieldClass) -> String {
        let key = postgres_string_literal(field);
        let value = match class {
            FieldClass::Number => format!("CAST(fields ->> {key} AS double precision)"),
            FieldClass::Text => format!("((fields ->> {key}) COLLATE \"C\")"),
            FieldClass::Boolean => format!("(fields ->> {key})"),
        };
        format!(
            "(CASE WHEN jsonb_typeof(fields -> {key}) = {} THEN {value} END)",
            json_type(class)
        )
    }

    fn literal(&self, placeholder: &str, class: FieldClass) -> String {
        match class {
            FieldClass::Number => format!("CAST({placeholder} AS double precision)"),
            FieldClass::Text => format!("({placeholder} COLLATE \"C\")"),
            FieldClass::Boolean => format!("CAST({placeholder} AS text)"),
        }
    }

    fn record_contains(&self, field: &str, literal: &str) -> String {
        let key = postgres_string_literal(field);
        format!(
            "strpos((COALESCE(state -> 'fields' ->> {key}, fields ->> {key}) +             COLLATE \"C\"), {literal}) > 0"
        )
    }

    fn status(&self) -> String {
        "(status COLLATE \"C\")".to_string()
    }
}
