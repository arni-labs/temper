//! Catalog column and record time predicates.
use super::super::*;
use temper_odata::query::types::{FilterExpr, ODataValue};

fn compare(field: &str, op: BinaryOperator, value: ODataValue) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(field.into())),
        op,
        right: Box::new(FilterExpr::Literal(value)),
    }
}

#[test]
fn status_ne_is_a_lossless_catalog_predicate() {
    let filter = compare(
        "status",
        BinaryOperator::Ne,
        ODataValue::String("Unmade".into()),
    );
    let result = try_translate_candidate_filter(&filter).unwrap();
    assert_eq!(result.where_clause, "status != ?3");
    assert!(candidate_filter_is_lossless(&filter));
}

#[test]
fn record_times_compare_as_nine_digit_utc_text() {
    let filter = compare(
        "last_event_at",
        BinaryOperator::Lt,
        ODataValue::String("2026-01-01T01:00:00.5+01:00".into()),
    );
    let result = try_translate_candidate_filter(&filter).unwrap();
    assert!(
        result.where_clause.contains("(state ->> 'last_event_at')"),
        "{}",
        result.where_clause
    );
    assert!(
        result.where_clause.ends_with("< ?3"),
        "{}",
        result.where_clause
    );
    assert_eq!(result.params, vec!["2026-01-01T00:00:00.500000000Z"]);
    assert!(candidate_filter_is_lossless(&filter));

    let unknown = compare("first_event_at", BinaryOperator::Eq, ODataValue::Null);
    let result = try_translate_candidate_filter(&unknown).unwrap();
    assert!(
        result.where_clause.ends_with("IS NULL"),
        "{}",
        result.where_clause
    );

    // Text that is not a time narrows the page but is rechecked in memory.
    let text = compare(
        "last_event_at",
        BinaryOperator::Gt,
        ODataValue::String("yesterday".into()),
    );
    assert!(!candidate_filter_is_lossless(&text));

    // Other kernel event metadata has no SQL form: it stays a recheck.
    let via = compare(
        "last_event_via",
        BinaryOperator::Eq,
        ODataValue::String("timer".into()),
    );
    assert!(try_translate_candidate_filter(&via).is_none());
}

#[test]
fn paging_keyset_terms_are_lossless() {
    let after_id = FilterExpr::BinaryOp {
        left: Box::new(compare(
            "entity_id",
            BinaryOperator::Gt,
            ODataValue::String("rec-1".into()),
        )),
        op: BinaryOperator::Or,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(compare(
                "entity_id",
                BinaryOperator::Eq,
                ODataValue::String("rec-1".into()),
            )),
            op: BinaryOperator::And,
            right: Box::new(FilterExpr::UnaryOp {
                op: UnaryOperator::Not,
                operand: Box::new(FilterExpr::Literal(ODataValue::Boolean(true))),
            }),
        }),
    };
    let result = try_translate_candidate_filter(&after_id).unwrap();
    assert_eq!(
        result.where_clause,
        "(entity_id > ?3 OR (entity_id = ?4 AND (NOT (1 = 1))))"
    );
    assert_eq!(result.params, vec!["rec-1", "rec-1"]);
    // `Id` is an app field in the index, not the catalog column.
    assert!(!candidate_filter_is_lossless(&compare(
        "Id",
        BinaryOperator::Gt,
        ODataValue::String("rec-1".into()),
    )));
}
