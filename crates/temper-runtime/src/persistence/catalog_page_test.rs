use super::*;
use crate::persistence::catalog_field::SketchSql;

#[test]
fn every_fraction_width_gives_one_fixed_width_key_in_time_order() {
    let times = [
        "2026-11-19T10:00:00Z",
        "2026-11-19T10:00:00.000000001Z",
        "2026-11-19T10:00:00.001Z",
        "2026-11-19T10:00:00.000002Z",
        "2026-11-19T10:00:00.5Z",
        "2026-11-19T12:00:00+02:00",
    ];
    let keys: Vec<String> = times
        .iter()
        .map(|time| event_time_key(time).expect("an RFC 3339 time has a key"))
        .collect();
    assert_eq!(keys[0], "2026-11-19T10:00:00.000000000");
    assert_eq!(keys[2], "2026-11-19T10:00:00.001000000");
    assert_eq!(keys[4], "2026-11-19T10:00:00.500000000");
    assert_eq!(keys[5], keys[0], "an offset is read as the same instant");
    assert!(keys.iter().all(|key| key.len() == EVENT_TIME_KEY_WIDTH));
    // The raw text of the first sorts after the second's; the keys do not.
    assert!(times[0] > times[1]);
    assert!(keys[0] < keys[1] && keys[1] < keys[3] && keys[3] < keys[2]);
    assert!(keys.iter().all(|key| key.as_str() < MISSING_EVENT_TIME_KEY));
    assert_eq!(MISSING_EVENT_TIME_KEY.len(), EVENT_TIME_KEY_WIDTH);
}

#[test]
fn text_that_is_not_a_time_has_no_key() {
    assert_eq!(event_time_key("yesterday"), None);
    assert_eq!(event_time_key("2026-11-19"), None);
}

fn order(field_name: &str, direction: QueryFieldIndexOrderDirection) -> QueryFieldIndexOrder {
    QueryFieldIndexOrder {
        field_name: field_name.to_string(),
        direction,
    }
}

#[test]
fn the_order_ends_with_entity_id_once() {
    let page = QueryFieldIndexPageQuery {
        order_by: vec![order("last_event_at", QueryFieldIndexOrderDirection::Desc)],
        ..QueryFieldIndexPageQuery::default()
    };
    assert_eq!(
        page.total_order(),
        vec![
            (CatalogOrderKey::EventTime(EventTimeColumn::Last), true),
            (CatalogOrderKey::EntityId, false),
        ]
    );
    let page = QueryFieldIndexPageQuery {
        order_by: vec![order("entity_id", QueryFieldIndexOrderDirection::Desc)],
        ..QueryFieldIndexPageQuery::default()
    };
    assert_eq!(page.total_order(), vec![(CatalogOrderKey::EntityId, true)]);
}

#[test]
fn a_keyset_seeks_its_first_key_and_resumes_strictly_after() {
    let page = QueryFieldIndexPageQuery {
        order_by: vec![order("last_event_at", QueryFieldIndexOrderDirection::Desc)],
        after: Some(vec!["2026-11-19T10:00:00.000000000".into(), "r-7".into()]),
        ..QueryFieldIndexPageQuery::default()
    };
    let (sql, params) = page.catalog_conditions(5, &SketchSql("K")).expect("keyset");
    assert_eq!(sql, "K <= ?5 AND (K < ?5 OR (K = ?5 AND entity_id > ?6))");
    assert_eq!(params, vec!["2026-11-19T10:00:00.000000000", "r-7"]);
}

#[test]
fn a_lower_bound_leaves_out_records_without_the_time() {
    let page = QueryFieldIndexPageQuery {
        event_time_bounds: vec![EventTimeBound {
            column: EventTimeColumn::First,
            comparison: KeyComparison::Ge,
            key: "2026-11-19T00:00:00.000000000".into(),
        }],
        ..QueryFieldIndexPageQuery::default()
    };
    let (sql, params) = page.catalog_conditions(3, &SketchSql("F")).expect("bounds");
    assert_eq!(sql, format!("F >= ?3 AND F < '{MISSING_EVENT_TIME_KEY}'"));
    assert_eq!(params.len(), 1);
}

#[test]
fn a_keyset_that_does_not_fit_the_order_is_refused() {
    let short = QueryFieldIndexPageQuery {
        order_by: vec![order("status", QueryFieldIndexOrderDirection::Asc)],
        after: Some(vec!["Open".into()]),
        ..QueryFieldIndexPageQuery::default()
    };
    assert!(short.catalog_conditions(3, &SketchSql("")).is_err());
    let field_by_text = QueryFieldIndexPageQuery {
        order_by: vec![order("Total", QueryFieldIndexOrderDirection::Asc)],
        after: Some(vec!["1".into(), "r-1".into()]),
        ..QueryFieldIndexPageQuery::default()
    };
    assert!(field_by_text.catalog_conditions(3, &SketchSql("")).is_err());
    let status_by_field = QueryFieldIndexPageQuery {
        order_by: vec![order("status", QueryFieldIndexOrderDirection::Asc)],
        after: Some(vec![KeysetValue::Field(None), "r-1".into()]),
        ..QueryFieldIndexPageQuery::default()
    };
    assert!(
        status_by_field
            .catalog_conditions(3, &SketchSql(""))
            .is_err()
    );
}

#[test]
fn a_field_keyset_puts_rows_without_a_value_last_ascending() {
    let page = |direction, cursor: Option<FieldLiteral>| QueryFieldIndexPageQuery {
        order_by: vec![order("rank", direction)],
        after: Some(vec![KeysetValue::Field(cursor), "r-1".into()]),
        ..QueryFieldIndexPageQuery::default()
    };
    let number = Some(FieldLiteral::Number("3".into()));
    let sql = |query: QueryFieldIndexPageQuery| {
        query.catalog_conditions(3, &SketchSql("")).expect("keyset")
    };
    let (asc, params) = sql(page(QueryFieldIndexOrderDirection::Asc, number.clone()));
    assert_eq!(
        asc,
        "((present(rank) AND (Number(rank) > Number(?3) OR (Number(rank) = Number(?3) \
         AND entity_id > ?4))) OR NOT present(rank))"
    );
    assert_eq!(params, vec!["3", "r-1"]);
    let (desc, _) = sql(page(QueryFieldIndexOrderDirection::Desc, number));
    assert_eq!(
        desc,
        "(present(rank) AND (Number(rank) < Number(?3) OR (Number(rank) = Number(?3) \
         AND entity_id > ?4)))"
    );
    let (asc_absent, params) = sql(page(QueryFieldIndexOrderDirection::Asc, None));
    assert_eq!(asc_absent, "(NOT present(rank) AND entity_id > ?3)");
    assert_eq!(params, vec!["r-1"]);
    let (desc_absent, _) = sql(page(QueryFieldIndexOrderDirection::Desc, None));
    assert_eq!(
        desc_absent,
        "(present(rank) OR (NOT present(rank) AND entity_id > ?3))"
    );
}

#[test]
fn mistyped_selects_rows_holding_a_value_of_another_class() {
    let page = QueryFieldIndexPageQuery {
        mistyped: vec![
            ("title".into(), FieldClass::Text),
            ("rank".into(), FieldClass::Number),
        ],
        ..QueryFieldIndexPageQuery::default()
    };
    let (sql, params) = page
        .catalog_conditions(3, &SketchSql(""))
        .expect("mistyped");
    assert_eq!(
        sql,
        "((has(title) AND NOT is_Text(title)) OR (has(rank) AND NOT is_Number(rank)))"
    );
    assert!(params.is_empty());
}
