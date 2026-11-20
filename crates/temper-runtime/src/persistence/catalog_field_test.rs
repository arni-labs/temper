use super::*;
use crate::persistence::QueryFieldIndexPageQuery;

fn comparison(op: FieldComparisonOp, literal: FieldLiteral, or_missing: bool) -> FieldComparison {
    FieldComparison {
        field: "rank".to_string(),
        op,
        literal,
        or_missing,
    }
}

#[test]
fn an_ordering_reads_only_values_of_the_literals_type() {
    let gt = comparison(
        FieldComparisonOp::Gt,
        FieldLiteral::Number("5".into()),
        false,
    );
    assert_eq!(
        field_comparison_sql(&SketchSql("K"), &gt, "?3"),
        "(is_Number(rank) AND Number(rank) > Number(?3))"
    );
}

#[test]
fn unequal_is_a_value_that_is_not_the_literal_of_its_type() {
    let ne = comparison(
        FieldComparisonOp::Ne,
        FieldLiteral::Text("steer".into()),
        false,
    );
    assert_eq!(
        field_comparison_sql(&SketchSql("K"), &ne, "?4"),
        "(present(rank) AND NOT (is_Text(rank) AND Text(rank) = Text(?4)))"
    );
}

#[test]
fn a_declared_default_lets_rows_without_the_key_through_for_the_reader() {
    let ne = comparison(
        FieldComparisonOp::Ne,
        FieldLiteral::Number("0".into()),
        true,
    );
    assert_eq!(
        field_comparison_sql(&SketchSql("K"), &ne, "?3"),
        "((present(rank) AND NOT (is_Number(rank) AND Number(rank) = Number(?3))) \
         OR NOT has(rank))"
    );
}

#[test]
fn a_page_binds_each_comparisons_literal_after_the_filters_parameters() {
    let page = QueryFieldIndexPageQuery {
        field_comparisons: vec![
            comparison(
                FieldComparisonOp::Ge,
                FieldLiteral::Text("2026-11-01".into()),
                false,
            ),
            comparison(FieldComparisonOp::Ne, FieldLiteral::Boolean(true), false),
        ],
        ..QueryFieldIndexPageQuery::default()
    };
    let (sql, params) = page
        .catalog_conditions(4, &SketchSql("K"))
        .expect("conditions");
    assert_eq!(
        sql,
        "(is_Text(rank) AND Text(rank) >= Text(?4)) AND \
         (present(rank) AND NOT (is_Boolean(rank) AND Boolean(rank) = Boolean(?5)))"
    );
    assert_eq!(params, vec!["2026-11-01", "true"]);
}
