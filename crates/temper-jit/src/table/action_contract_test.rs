//! Tests for action contracts: initial fields, params and related-field bindings.

use super::*;
use serde_json::json;

#[test]
fn strict_initial_identity_is_a_nonempty_string() {
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Identity"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
"#,
    );
    for invalid in [
        json!({"x":1}),
        json!([]),
        json!(1),
        json!(false),
        Value::Null,
        json!(""),
    ] {
        for key in ["Id", "id"] {
            assert!(
                table
                    .validate_initial_fields(&json!({key:invalid}))
                    .is_err()
            );
        }
    }
    assert!(
        table
            .validate_initial_fields(&json!({"Id":"valid","id":"valid"}))
            .is_ok()
    );
}

#[test]
fn constraints_do_not_recreate_missing_persisted_fields_from_defaults() {
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "PersistedSequence"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "sequence"
type = "counter"
initial = 7
[[action]]
name = "Advance"
kind = "input"
from = ["Active"]
params = ["expected"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected"
field = "sequence"
"#,
    );
    let params = json!({"expected":7});
    let mut fields = json!({});
    let mut counters = BTreeMap::new();
    let mut booleans = BTreeMap::new();
    assert!(
        table
            .validate_action_params("Advance", &params, &fields, &counters, &booleans)
            .is_err()
    );
    table.initialize_declared_fields(&mut fields, &mut counters, &mut booleans);
    assert!(
        table
            .validate_action_params("Advance", &params, &fields, &counters, &booleans)
            .is_ok()
    );
    counters.insert("sequence".into(), 8);
    assert!(
        table
            .validate_action_params("Advance", &params, &fields, &counters, &booleans)
            .is_err()
    );
}

#[test]
fn greater_than_requires_a_nonnegative_parameter_even_for_signed_state() {
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "SignedSequence"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "sequence"
type = "int"
initial = -3
[[action]]
name = "Advance"
kind = "input"
from = ["Active"]
params = ["expected"]
[[action.constraints]]
kind = "param_greater_than_field"
param = "expected"
field = "sequence"
"#,
    );
    let fields = json!({"sequence":-3});
    let counters = BTreeMap::new();
    let booleans = BTreeMap::new();
    assert!(
        table
            .validate_action_params(
                "Advance",
                &json!({"expected":-2}),
                &fields,
                &counters,
                &booleans
            )
            .is_err()
    );
    assert!(
        table
            .validate_action_params(
                "Advance",
                &json!({"expected":0}),
                &fields,
                &counters,
                &booleans
            )
            .is_ok()
    );
}

#[test]
fn a_param_bound_to_a_related_field_must_equal_that_record_s_value() {
    use crate::table::{Related, RelatedMap, RelatedRecord};
    let table = TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "Child"
states = ["Waiting", "Done"]
initial = "Waiting"
[[state]]
name = "value"
type = "string"
initial = ""
[[action]]
name = "Copy"
kind = "internal"
from = ["Waiting", "Done"]
to = "Done"
params = [{ name = "value", type = "string" }]
[[action.constraints]]
kind = "param_equals_related_field"
param = "value"
entity = "Parent"
id_field = "Id"
field = "value"
"#,
    );
    assert_eq!(
        table.constraint_related_refs("Copy"),
        [("Parent".to_string(), "Id".to_string(), "value".to_string())]
    );
    let parent_holds = |value: &str| -> RelatedMap {
        RelatedMap::from([(
            ("Parent".to_string(), "Id".to_string()),
            Related::Records(vec![Some(RelatedRecord {
                status: "Open".to_string(),
                fields: BTreeMap::from([("value".to_string(), json!(value))]),
            })]),
        )])
    };
    assert!(
        table
            .validate_related_params("Copy", &json!({"value": "a"}), &parent_holds("a"))
            .is_ok()
    );
    let wrong = table
        .validate_related_params("Copy", &json!({"value": "b"}), &parent_holds("a"))
        .unwrap_err();
    assert_eq!(
        wrong.refusal,
        Some(Refusal::ParameterConstraint {
            parameter: "value".to_string()
        })
    );
    // Not read, or not found: the value cannot be shown to match.
    assert!(
        table
            .validate_related_params("Copy", &json!({"value": "a"}), &RelatedMap::new())
            .is_err()
    );
    let missing = RelatedMap::from([(
        ("Parent".to_string(), "Id".to_string()),
        Related::Records(vec![None]),
    )]);
    assert!(
        table
            .validate_related_params("Copy", &json!({"value": "a"}), &missing)
            .is_err()
    );
    assert!(
        table
            .validate_related_params("Copy", &json!({}), &parent_holds("a"))
            .is_err()
    );
}
