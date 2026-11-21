//! How a catalog condition reads a row in Turso's SQL (ADR-0188).
//!
//! `json_type` names a field's JSON type (`integer`, `real`, `text`, `true`,
//! `false`, `null`, `array`, `object`), or is NULL when `fields` has no such
//! key. A number is compared as a double: both sides are cast to `REAL`, and
//! the literal is read by the JSON parser (`json_extract(?, '$')`), which
//! parses a number as Rust does, so it is the double the evaluator holds.

use temper_runtime::persistence::{CatalogSql, EventTimeColumn, FieldClass};

use super::query_page::turso_json_path_literal;
use crate::schema::event_time_key_sql;

/// Turso's [`CatalogSql`].
pub(crate) struct TursoCatalogSql;

fn json_types(class: FieldClass) -> &'static str {
    match class {
        FieldClass::Number => "('integer', 'real')",
        FieldClass::Text => "('text')",
        FieldClass::Boolean => "('true', 'false')",
    }
}

impl CatalogSql for TursoCatalogSql {
    fn event_time_key(&self, column: EventTimeColumn) -> String {
        event_time_key_sql(column)
    }

    fn field_has_key(&self, field: &str) -> String {
        let path = turso_json_path_literal(field);
        format!("(json_type(fields, {path}) IS NOT NULL)")
    }

    fn field_present(&self, field: &str) -> String {
        let path = turso_json_path_literal(field);
        format!("(COALESCE(json_type(fields, {path}), 'null') <> 'null')")
    }

    fn field_is(&self, field: &str, class: FieldClass) -> String {
        let path = turso_json_path_literal(field);
        format!(
            "(COALESCE(json_type(fields, {path}), '') IN {})",
            json_types(class)
        )
    }

    fn field_value(&self, field: &str, class: FieldClass) -> String {
        let path = turso_json_path_literal(field);
        let value = match class {
            FieldClass::Number => format!("CAST(json_extract(fields, {path}) AS REAL)"),
            FieldClass::Text => format!("json_extract(fields, {path})"),
            FieldClass::Boolean => format!("json_type(fields, {path})"),
        };
        format!(
            "(CASE WHEN json_type(fields, {path}) IN {} THEN {value} END)",
            json_types(class)
        )
    }

    fn literal(&self, placeholder: &str, class: FieldClass) -> String {
        match class {
            FieldClass::Number => format!("CAST(json_extract({placeholder}, '$') AS REAL)"),
            FieldClass::Text | FieldClass::Boolean => placeholder.to_string(),
        }
    }

    fn record_contains(&self, field: &str, literal: &str) -> String {
        let field_path = turso_json_path_literal(field);
        let state_path = field_path.replacen("'$.", "'$.fields.", 1);
        format!(
            "instr(COALESCE(json_extract(state, {state_path}), +             json_extract(fields, {field_path})), {literal}) > 0"
        )
    }

    fn record_json_contains(&self, literal: &str) -> String {
        format!("(instr(fields, {literal}) > 0 OR instr(state, {literal}) > 0)")
    }

    /// The column, under its default `BINARY` collation: byte order.
    fn status(&self) -> String {
        "status".to_string()
    }
}
