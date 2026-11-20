use super::*;
use crate::persistence::catalog_field::SketchSql;

fn query(key: FacetKey, by_count: bool, after: Option<FacetAfter>) -> CatalogFacetQuery {
    CatalogFacetQuery {
        filter: QueryFieldIndexPageQuery {
            where_clause: "status = ?3".to_string(),
            params: vec!["Open".to_string()],
            ..QueryFieldIndexPageQuery::default()
        },
        key,
        by_count,
        after,
        top: 10,
    }
}

fn rank() -> FacetKey {
    FacetKey::Field {
        name: "rank".to_string(),
        class: FieldClass::Number,
    }
}

#[test]
fn a_first_page_groups_the_filtered_rows_by_value_with_none_last() {
    let (statement, params) = query(rank(), false, None)
        .statement("cat", &SketchSql(".0"))
        .expect("a statement");
    assert_eq!(
        statement,
        "SELECT g, n FROM (SELECT Number(rank) AS g, COUNT(*) AS n FROM cat \
         WHERE tenant = ?1 AND entity_type = ?2 AND (status = ?3) AND TRUE \
         GROUP BY Number(rank)) facets \
         WHERE TRUE ORDER BY (g IS NULL), g LIMIT ?4"
    );
    assert_eq!(params, vec!["Open".to_string()]);
}

#[test]
fn a_count_order_resumes_after_the_last_groups_count_and_value() {
    let after = FacetAfter {
        count: 7,
        value: Some(FieldLiteral::Text("Open".to_string())),
    };
    let (statement, params) = query(FacetKey::Status, true, Some(after))
        .statement("cat", &SketchSql(".0"))
        .expect("a statement");
    assert!(
        statement.ends_with(
            "WHERE (n < CAST(?5 AS BIGINT) OR (n = CAST(?5 AS BIGINT) AND \
             (g IS NULL OR g > Text(?4)))) ORDER BY n DESC, (g IS NULL), g LIMIT ?6"
        ),
        "{statement}"
    );
    assert!(statement.contains("SELECT status AS g"), "{statement}");
    assert_eq!(params, vec!["Open", "Open", "7"]);
}

#[test]
fn no_value_follows_the_group_without_one() {
    let after = FacetAfter {
        count: 2,
        value: None,
    };
    let (statement, _) = query(rank(), false, Some(after))
        .statement("cat", &SketchSql(".0"))
        .expect("a statement");
    assert!(statement.contains("WHERE FALSE ORDER BY"), "{statement}");
}

#[test]
fn a_cursor_value_of_another_type_is_refused() {
    let after = FacetAfter {
        count: 2,
        value: Some(FieldLiteral::Text("3".to_string())),
    };
    assert!(
        query(rank(), false, Some(after))
            .statement("cat", &SketchSql(".0"))
            .is_err()
    );
}
