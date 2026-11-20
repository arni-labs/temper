//! Top-level `and` conjuncts of a `$filter` that compare a declared app
//! field with a literal of its own type, read as comparisons on each row's
//! typed JSON value (ADR-0188).
//!
//! `query_eval` compares the value a record reads as: a number with a number
//! as a double, a string with a string byte by byte, a boolean only for
//! equality; a record with no value, or a JSON null, meets no comparison,
//! and a value of another JSON type is unequal and meets no ordering. The
//! stores make the same comparison on the row's `fields`
//! (`temper_runtime::persistence::field_comparison_sql`), so a conjunct read
//! here is exact in SQL. Three things could make a record read differently
//! from its catalog row's `fields`, and each keeps a field out:
//!
//! - a property of the same name at the top of the record, which the
//!   evaluator reads first (`resolve_property`): [`RECORD_PROPERTIES`];
//! - a field the spec keeps out of the query projection
//!   (`query_indexed = false`), which `fields` does not hold;
//! - a declared default, which a record without the key reads as: such a
//!   comparison also lets those rows through (`or_missing`), and the read
//!   checks them in memory unless no candidate lacks the key.

use std::collections::{BTreeMap, BTreeSet};

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};
use temper_runtime::persistence::{FieldClass, FieldComparison, FieldComparisonOp, FieldLiteral};

use super::event_time::collect_conjuncts;

/// The properties every materialized record holds at its top, which a
/// property of the same name in `fields` cannot be read past: the catalog
/// row's own columns and what the kernel keeps of a record's state.
pub(in crate::odata) const RECORD_PROPERTIES: [&str; 18] = [
    "entity_type",
    "entity_id",
    "status",
    "Status",
    "item_count",
    "counters",
    "booleans",
    "lists",
    "fields",
    "spawn_reservations",
    "events",
    "total_event_count",
    "events_since_snapshot",
    "last_snapshot_sequence_nr",
    "sequence_nr",
    "processed_idempotency_keys",
    "status_clock",
    "@odata.id",
];

/// The app fields a read may compare on typed values: each declared,
/// non-nullable scalar property with its JSON type, and which of them have
/// a declared default.
#[derive(Debug, Default)]
pub(in crate::odata) struct DeclaredFields {
    classes: BTreeMap<String, FieldClass>,
    defaulted: BTreeSet<String>,
}

impl DeclaredFields {
    /// A field that compares as `class`.
    pub(in crate::odata) fn declare(&mut self, name: &str, class: FieldClass) {
        let reserved = RECORD_PROPERTIES.contains(&name)
            || temper_spec::automaton::is_event_metadata_field(name);
        if !reserved {
            self.classes.insert(name.to_string(), class);
        }
    }

    /// A field a record without the key reads as its declared default.
    pub(in crate::odata) fn default_for(&mut self, name: &str) {
        self.defaulted.insert(name.to_string());
    }

    /// The JSON type `name` compares as, when it may be compared.
    pub(in crate::odata) fn class_of(&self, name: &str) -> Option<FieldClass> {
        self.classes.get(name).copied()
    }

    fn has_default(&self, name: &str) -> bool {
        self.defaulted.contains(name)
    }
}

/// The JSON type a declared Edm type holds, for the types a comparison or an
/// order reads exactly.
pub(in crate::odata) fn edm_class(type_name: &str) -> Option<FieldClass> {
    match type_name {
        "Edm.Byte" | "Edm.SByte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64" | "Edm.Decimal"
        | "Edm.Double" | "Edm.Single" => Some(FieldClass::Number),
        "Edm.String" | "Edm.Guid" | "Edm.Date" | "Edm.DateTimeOffset" | "Edm.TimeOfDay" => {
            Some(FieldClass::Text)
        }
        "Edm.Boolean" => Some(FieldClass::Boolean),
        _ => None,
    }
}

/// A filter's top-level conjuncts, split into typed field comparisons and
/// the rest.
pub(in crate::odata) struct SplitFields {
    pub(in crate::odata) comparisons: Vec<FieldComparison>,
    /// Defaulted fields among the comparisons: rows without their key are
    /// candidates the reader checks.
    pub(in crate::odata) missing_default_fields: BTreeSet<String>,
    /// The other conjuncts, in their order; `None` when every one was read.
    pub(in crate::odata) rest: Option<FilterExpr>,
}

/// Split `filter` into the comparisons `declared` lets SQL make exactly and
/// the conjuncts left over.
pub(in crate::odata) fn split_field_comparisons(
    filter: &FilterExpr,
    declared: &DeclaredFields,
) -> SplitFields {
    let mut conjuncts = Vec::new();
    collect_conjuncts(filter, &mut conjuncts);
    let mut split = SplitFields {
        comparisons: Vec::new(),
        missing_default_fields: BTreeSet::new(),
        rest: None,
    };
    for conjunct in conjuncts {
        if let Some(mut comparison) = typed_comparison(conjunct, declared) {
            if declared.has_default(&comparison.field) {
                comparison.or_missing = true;
                split
                    .missing_default_fields
                    .insert(comparison.field.clone());
            }
            split.comparisons.push(comparison);
            continue;
        }
        split.rest = Some(match split.rest.take() {
            Some(left) => FilterExpr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(conjunct.clone()),
            },
            None => conjunct.clone(),
        });
    }
    split
}

/// `field op literal`, either way round, when the field compares as the
/// literal's type and SQL makes that comparison as the evaluator does.
fn typed_comparison(conjunct: &FilterExpr, declared: &DeclaredFields) -> Option<FieldComparison> {
    let FilterExpr::BinaryOp { left, op, right } = conjunct else {
        return None;
    };
    let op = comparison_op(*op)?;
    let (field, literal, op) = match (left.as_ref(), right.as_ref()) {
        (FilterExpr::Property(field), FilterExpr::Literal(literal)) => (field, literal, op),
        (FilterExpr::Literal(literal), FilterExpr::Property(field)) => {
            (field, literal, flipped(op))
        }
        _ => return None,
    };
    let literal = field_literal(literal)?;
    let class = declared.class_of(field)?;
    (literal.class() == class && typed_in_sql(class, op)).then(|| FieldComparison {
        field: field.clone(),
        op,
        literal,
        or_missing: false,
    })
}

/// Which comparisons of a type are read here. Equality of a string or a
/// boolean is left to the field index's text (`filter_sql`), which an index
/// lookup answers. A boolean meets no ordering in the evaluator.
fn typed_in_sql(class: FieldClass, op: FieldComparisonOp) -> bool {
    match class {
        FieldClass::Text | FieldClass::Boolean => op == FieldComparisonOp::Ne,
        FieldClass::Number => false,
    }
}

fn comparison_op(op: BinaryOperator) -> Option<FieldComparisonOp> {
    Some(match op {
        BinaryOperator::Eq => FieldComparisonOp::Eq,
        BinaryOperator::Ne => FieldComparisonOp::Ne,
        BinaryOperator::Lt => FieldComparisonOp::Lt,
        BinaryOperator::Le => FieldComparisonOp::Le,
        BinaryOperator::Gt => FieldComparisonOp::Gt,
        BinaryOperator::Ge => FieldComparisonOp::Ge,
        BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Has => return None,
    })
}

/// `literal op field` is `field flipped(op) literal`.
fn flipped(op: FieldComparisonOp) -> FieldComparisonOp {
    match op {
        FieldComparisonOp::Lt => FieldComparisonOp::Gt,
        FieldComparisonOp::Le => FieldComparisonOp::Ge,
        FieldComparisonOp::Gt => FieldComparisonOp::Lt,
        FieldComparisonOp::Ge => FieldComparisonOp::Le,
        op => op,
    }
}

/// The literal as the evaluator holds it (`query_eval::odata_value_to_json`):
/// a GUID and a time are their text. A float that is not finite is a JSON
/// null there, so it is not read.
fn field_literal(literal: &ODataValue) -> Option<FieldLiteral> {
    Some(match literal {
        ODataValue::String(text) => FieldLiteral::Text(text.clone()),
        ODataValue::Guid(guid) => FieldLiteral::Text(guid.to_string()),
        ODataValue::DateTimeOffset(time) => FieldLiteral::Text(time.to_rfc3339()),
        ODataValue::Int(value) => FieldLiteral::Number(value.to_string()),
        ODataValue::Float(value) if value.is_finite() => FieldLiteral::Number(value.to_string()),
        ODataValue::Boolean(value) => FieldLiteral::Boolean(*value),
        ODataValue::Float(_) | ODataValue::Null => return None,
    })
}

#[cfg(test)]
#[path = "field_compare_test.rs"]
mod tests;
