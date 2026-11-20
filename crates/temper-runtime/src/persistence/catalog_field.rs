//! App-field comparisons a catalog page makes on each row's typed JSON value
//! (ADR-0188).
//!
//! The field index keeps every scalar as text, so a number and a string with
//! the same text look alike there, and an order or a range on numbers in it
//! compares text. The in-memory evaluator (`query_eval` in temper-server)
//! compares JSON values: a number with a number as an IEEE double, a string
//! with a string byte by byte, a boolean only for equality, and a value of
//! another JSON type never compares at all. A comparison here reads the same
//! JSON value from the row's `fields` and makes exactly that comparison, so
//! each store answers what the evaluator answers for every stored value.
//!
//! Each store renders a field's JSON type and value in its own SQL
//! ([`CatalogSql`]); the comparisons are put together here, so every store
//! compares the same way.

use super::catalog_page::EventTimeColumn;

/// The JSON type a comparison reads a field as.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum FieldClass {
    /// A JSON number, compared as an IEEE double.
    Number,
    /// A JSON string, compared byte by byte.
    Text,
    /// A JSON boolean, compared only for equality.
    Boolean,
}

/// A literal a field is compared with, in the JSON type it compares as.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FieldLiteral {
    /// A number, as JSON number text (`5`, `-0.25`, `1e300`).
    Number(String),
    /// A string.
    Text(String),
    /// A boolean.
    Boolean(bool),
}

impl FieldLiteral {
    /// The JSON type this literal compares as.
    pub fn class(&self) -> FieldClass {
        match self {
            Self::Number(_) => FieldClass::Number,
            Self::Text(_) => FieldClass::Text,
            Self::Boolean(_) => FieldClass::Boolean,
        }
    }

    /// The parameter the store binds for this literal: the number's JSON
    /// text, the string, or `true`/`false`.
    pub fn param(&self) -> String {
        match self {
            Self::Number(text) | Self::Text(text) => text.clone(),
            Self::Boolean(value) => value.to_string(),
        }
    }
}

/// How a field compares with its literal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldComparisonOp {
    /// Equal.
    Eq,
    /// Not equal: the field holds a value, and it is not the literal.
    Ne,
    /// Less than.
    Lt,
    /// At most.
    Le,
    /// Greater than.
    Gt,
    /// At least.
    Ge,
}

impl FieldComparisonOp {
    fn sql(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::Ne => "<>",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// `field op literal` on the row's own JSON value, as the evaluator decides
/// it: a field with no value, or a JSON null, meets no comparison; a value of
/// another JSON type is unequal to the literal and meets no ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FieldComparison {
    /// The app field compared.
    pub field: String,
    /// How it compares.
    pub op: FieldComparisonOp,
    /// What it compares with.
    pub literal: FieldLiteral,
    /// Rows that hold no key for the field pass as well: a declared default
    /// decides them when the record is read, so the reader checks them.
    pub or_missing: bool,
}

/// How one store writes the parts of a catalog condition in its SQL.
/// Every expression reads the row of `entity_catalog` the statement is on.
pub trait CatalogSql {
    /// The sort key of an event time (ADR-0187).
    fn event_time_key(&self, column: EventTimeColumn) -> String;
    /// True when the row's `fields` holds a key for `field`, null included.
    fn field_has_key(&self, field: &str) -> String;
    /// True when the row's `fields` holds a value for `field` that is not a
    /// JSON null. Never NULL itself.
    fn field_present(&self, field: &str) -> String;
    /// True when that value is of `class`. Never NULL itself.
    fn field_is(&self, field: &str, class: FieldClass) -> String;
    /// The value as `class` compares it, NULL when it is of another type:
    /// a double for a number, text under byte order for a string, and the
    /// text `true` or `false` for a boolean.
    fn field_value(&self, field: &str, class: FieldClass) -> String;
    /// The parameter `placeholder` as `class` compares it, from
    /// [`FieldLiteral::param`].
    fn literal(&self, placeholder: &str, class: FieldClass) -> String;
}

/// The SQL condition of `comparison`, with its literal at `placeholder`.
pub fn field_comparison_sql(
    sql: &dyn CatalogSql,
    comparison: &FieldComparison,
    placeholder: &str,
) -> String {
    let field = comparison.field.as_str();
    let class = comparison.literal.class();
    let is = sql.field_is(field, class);
    let value = sql.field_value(field, class);
    let literal = sql.literal(placeholder, class);
    let condition = match comparison.op {
        // Unequal: it holds a value, and that value is not the literal of
        // the literal's type. A value of another type is unequal.
        FieldComparisonOp::Ne => format!(
            "({} AND NOT ({is} AND {value} = {literal}))",
            sql.field_present(field)
        ),
        op => format!("({is} AND {value} {} {literal})", op.sql()),
    };
    if comparison.or_missing {
        format!("({condition} OR NOT {})", sql.field_has_key(field))
    } else {
        condition
    }
}

/// A store dialect for unit tests: each expression is a short readable name,
/// and every event time's key reads as `.0`.
#[cfg(test)]
pub(crate) struct SketchSql(pub &'static str);

#[cfg(test)]
impl CatalogSql for SketchSql {
    fn event_time_key(&self, _column: EventTimeColumn) -> String {
        self.0.to_string()
    }
    fn field_has_key(&self, field: &str) -> String {
        format!("has({field})")
    }
    fn field_present(&self, field: &str) -> String {
        format!("present({field})")
    }
    fn field_is(&self, field: &str, class: FieldClass) -> String {
        format!("is_{class:?}({field})")
    }
    fn field_value(&self, field: &str, class: FieldClass) -> String {
        format!("{class:?}({field})")
    }
    fn literal(&self, placeholder: &str, class: FieldClass) -> String {
        format!("{class:?}({placeholder})")
    }
}

#[cfg(test)]
#[path = "catalog_field_test.rs"]
mod tests;
