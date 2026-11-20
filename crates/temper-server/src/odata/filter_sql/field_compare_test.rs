use super::*;
use temper_odata::query::types::parse_query_options;

fn filter(text: &str) -> FilterExpr {
    parse_query_options(&format!("$filter={text}"))
        .expect("filter parses")
        .filter
        .expect("a filter")
}

fn declared() -> DeclaredFields {
    let mut declared = DeclaredFields::default();
    declared.declare("kind", FieldClass::Text);
    declared.declare("pinned", FieldClass::Boolean);
    declared.declare("rank", FieldClass::Number);
    declared.declare("note", FieldClass::Text);
    declared.default_for("note");
    declared
}

#[test]
fn unequal_on_a_declared_string_or_boolean_is_read_as_a_typed_comparison() {
    let split = split_field_comparisons(
        &filter("kind ne 'steer' and 'x' ne note and pinned ne true and Status eq 'Open'"),
        &declared(),
    );
    assert_eq!(
        split.comparisons,
        vec![
            FieldComparison {
                field: "kind".into(),
                op: FieldComparisonOp::Ne,
                literal: FieldLiteral::Text("steer".into()),
                or_missing: false,
            },
            FieldComparison {
                field: "note".into(),
                op: FieldComparisonOp::Ne,
                literal: FieldLiteral::Text("x".into()),
                or_missing: true,
            },
            FieldComparison {
                field: "pinned".into(),
                op: FieldComparisonOp::Ne,
                literal: FieldLiteral::Boolean(true),
                or_missing: false,
            },
        ]
    );
    assert_eq!(
        split.missing_default_fields.into_iter().collect::<Vec<_>>(),
        vec!["note".to_string()]
    );
    assert_eq!(split.rest, Some(filter("Status eq 'Open'")));
}

#[test]
fn a_literal_of_another_type_an_undeclared_field_or_an_or_stays_in_the_filter() {
    for text in [
        "kind ne 5",
        "other ne 'x'",
        "kind ne 'a' or kind ne 'b'",
        "kind eq 'steer'",
        "kind ne null",
    ] {
        let split = split_field_comparisons(&filter(text), &declared());
        assert!(split.comparisons.is_empty(), "{text}");
        assert_eq!(split.rest, Some(filter(text)), "{text}");
    }
}

#[test]
fn a_property_the_record_holds_at_its_top_is_never_compared_in_fields() {
    let mut declared = DeclaredFields::default();
    declared.declare("sequence_nr", FieldClass::Number);
    declared.declare("last_event_at", FieldClass::Text);
    assert_eq!(declared.class_of("sequence_nr"), None);
    assert_eq!(declared.class_of("last_event_at"), None);
}

/// Every property a record's state is written with sits at the top of the
/// materialized record, where `resolve_property` reads it before `fields`.
#[test]
fn record_properties_cover_every_top_level_key_of_a_record() {
    let state: crate::entity_actor::EntityState = serde_json::from_value(serde_json::json!({
        "entity_type": "T", "entity_id": "e", "status": "S", "item_count": 0, "fields": {},
        "spawn_reservations": {"k": "v"}
    }))
    .expect("a minimal state");
    let written = serde_json::to_value(&state).expect("state serializes");
    for key in written.as_object().expect("an object").keys() {
        assert!(
            RECORD_PROPERTIES.contains(&key.as_str())
                || temper_spec::automaton::is_event_metadata_field(key),
            "{key} is read from the top of a record"
        );
    }
}
