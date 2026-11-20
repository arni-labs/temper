use super::*;
use serde_json::json;

fn key(value: serde_json::Value) -> GroupKey {
    GroupKey::of(Some(&value))
}

#[test]
fn numbers_group_as_doubles_and_print_as_integers_when_integral() {
    assert_eq!(key(json!(3)), key(json!(3.0)));
    assert_eq!(key(json!(-0.0)), key(json!(0)));
    assert_eq!(key(json!(3.0)).to_json(), json!(3));
    assert_eq!(key(json!(0.5)).to_json(), json!(0.5));
    assert_eq!(key(json!(1e300)).to_json(), json!(1e300));
    assert!(key(json!(2)) < key(json!(10)));
}

#[test]
fn values_sort_by_type_then_value_with_the_null_group_last() {
    let mut keys = [
        GroupKey::of(None),
        key(json!("b")),
        key(json!([1])),
        key(json!(5)),
        key(json!("B")),
        key(json!(true)),
        key(json!(null)),
    ];
    keys.sort();
    assert_eq!(
        keys.iter().map(GroupKey::to_json).collect::<Vec<_>>(),
        vec![
            json!(true),
            json!(5),
            json!("B"),
            json!("b"),
            json!([1]),
            json!(null),
            json!(null)
        ]
    );
}

#[test]
fn a_count_order_puts_the_largest_first_and_breaks_ties_by_value() {
    let group = |value: serde_json::Value, count| Group {
        key: key(value),
        count,
    };
    let mut groups = [
        group(json!("b"), 2),
        group(json!("c"), 5),
        group(json!("a"), 2),
    ];
    groups.sort_by(|a, b| a.order(b, true));
    let order: Vec<_> = groups.iter().map(|g| g.key.to_json()).collect();
    assert_eq!(order, vec![json!("c"), json!("a"), json!("b")]);
}

#[test]
fn a_cursor_round_trips_for_its_own_property_only() {
    let group = Group {
        key: key(json!(2.5)),
        count: 4,
    };
    let token = group.cursor("rank");
    assert_eq!(Group::from_cursor(&token, "rank"), Some(group.clone()));
    assert_eq!(Group::from_cursor(&token, "kind"), None);
    assert_eq!(Group::from_cursor("not a cursor", "rank"), None);
    assert_eq!(
        group.facet_after(FieldClass::Number),
        Ok(FacetAfter {
            count: 4,
            value: Some(FieldLiteral::Number("2.5".to_string())),
        })
    );
    assert_eq!(group.facet_after(FieldClass::Text), Err(()));
}
