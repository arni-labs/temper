//! Validate parameter constraints before a specification is installed.
use super::{ActionConstraint, ActionParam, Automaton, VarType, parser::AutomatonParseError};
use std::collections::{BTreeMap, BTreeSet};

/// What a constraint compares a value of this type as.
fn category(var_type: VarType) -> &'static str {
    match var_type {
        VarType::Counter | VarType::Int => "integer",
        VarType::String => "string",
        VarType::Bool => "bool",
        VarType::List => "list",
    }
}

pub(super) fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    let contracted = automaton.automaton.strict_action_params
        || automaton
            .actions
            .iter()
            .any(|action| !action.constraints.is_empty());
    let mut names = BTreeSet::new();
    if contracted {
        for action in &automaton.actions {
            if !names.insert(&action.name) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' has more than one parameter contract",
                    action.name
                )));
            }
        }
    }
    for action in &automaton.actions {
        let mut implied_types = BTreeMap::new();
        for constraint in &action.constraints {
            let fail = |reason: &str| {
                AutomatonParseError::Validation(format!(
                    "action '{}' constraint for '{}': {reason}",
                    action.name,
                    constraint.param()
                ))
            };
            let param = action
                .params
                .iter()
                .find(|param| param.name() == constraint.param())
                .ok_or_else(|| fail("parameter is undeclared"))?;
            let field_type = if let Some(name) = constraint.field() {
                Some(
                    if let Some(field) = automaton.state.iter().find(|field| field.name == name) {
                        category(field.var_type)
                    } else if matches!(name, "Id" | "id") {
                        "string"
                    } else {
                        return Err(fail("field is undeclared"));
                    },
                )
            } else {
                None
            };
            if field_type == Some("list") {
                return Err(fail("collection field comparisons are not supported"));
            }
            if field_type.is_some_and(|kind| !matches!(kind, "string" | "bool" | "integer")) {
                return Err(fail(
                    "field comparisons require a string, boolean or integer",
                ));
            }
            let required_type = match constraint {
                ActionConstraint::ParamGreaterThanField { .. } => {
                    if field_type != Some("integer") {
                        return Err(fail("greater-than requires an integer field"));
                    }
                    Some("integer")
                }
                ActionConstraint::ParamNonempty { .. } => Some("string"),
                _ => field_type,
            };
            if let Some(required) = required_type
                && let Some(previous) = implied_types.insert(constraint.param(), required)
                && previous != required
            {
                return Err(fail("parameter constraints require incompatible types"));
            }
            if let ActionParam::Typed { param_type, .. } = param
                && required_type.is_some_and(|expected| category(*param_type) != expected)
            {
                return Err(fail(
                    "declared parameter type does not match the constraint",
                ));
            }
        }
        if automaton.automaton.strict_action_params || !action.constraints.is_empty() {
            let mut names = BTreeSet::new();
            for param in &action.params {
                if !names.insert(param.name()) {
                    return Err(AutomatonParseError::Validation(format!(
                        "action '{}' declares parameter '{}' more than once",
                        action.name,
                        param.name(),
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::parse_automaton;

    /// The typed starting value of a `field_type` variable.
    fn initial_for(field_type: &str) -> &'static str {
        match field_type {
            "counter" | "int" => "0",
            "bool" => "false",
            "list" => "[]",
            _ => "\"\"",
        }
    }

    fn spec(field_type: &str, param: &str, kind: &str) -> String {
        let initial = initial_for(field_type);
        format!(
            r#"
[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
[[state]]
name = "sequence"
type = "{field_type}"
initial = {initial}
[[action]]
name = "Observe"
from = ["Ready"]
params = [{param}]
[[action.constraints]]
kind = "{kind}"
param = "value"
field = "sequence"
"#
        )
    }

    #[test]
    fn counter_parameters_compare_with_counter_fields() {
        for param in [r#""value""#, r#"{name="value",type="counter"}"#] {
            for kind in [
                "param_equals_field",
                "param_not_equals_field",
                "param_greater_than_field",
            ] {
                assert!(parse_automaton(&spec("counter", param, kind)).is_ok());
                assert!(parse_automaton(&spec("uint64", param, kind)).is_err());
            }
        }
    }

    #[test]
    fn malformed_typed_parameter_declarations_are_never_bare_parameter_names() {
        let base = spec(
            "counter",
            r#"{name="value",type="counter"}"#,
            "param_equals_field",
        );
        let source = base.split("[[action.constraints]]").next().unwrap();
        for malformed in [
            r#"{name="value",type="counter"}"#,
            r#"[{name="value",type="counter",}]"#,
            r#"[{name="value",type=7}]"#,
            r#"[{name=7,type="counter"}]"#,
            r#"[{name="value",type="uint64"}]"#,
            r#"[{name="value",kind="counter"}]"#,
        ] {
            let invalid = source.replace(r#"[{name="value",type="counter"}]"#, malformed);
            assert!(parse_automaton(&invalid).is_err(), "accepted {malformed}");
        }
    }

    #[test]
    fn rejects_unsupported_or_duplicate_explicit_parameter_contracts() {
        let base = spec(
            "string",
            r#"{name="value",type="string"}"#,
            "param_equals_field",
        );
        let unconstrained = base.split("[[action.constraints]]").next().unwrap();
        assert!(parse_automaton(unconstrained).is_ok());
        assert!(
            parse_automaton(&unconstrained.replace("type=\"string\"", "type=\"unknown\"")).is_err()
        );
        assert!(
            parse_automaton(&unconstrained.replace(
                r#"{name="value",type="string"}"#,
                r#"{name="value",type="string"}, {name="value",type="bool"}"#,
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_collection_field_comparisons_before_installation() {
        for kind in ["param_equals_field", "param_not_equals_field"] {
            let source = spec("list", r#"{name="value",type="list"}"#, kind);
            let error = parse_automaton(&source)
                .expect_err("collection comparison has no runtime contract");
            assert!(error.to_string().contains("collection field"), "{error}");
        }
    }

    #[test]
    fn bare_parameters_must_have_consistent_constraint_types() {
        let source = spec("counter", "\"value\"", "param_equals_field");
        let conflicting =
            format!("{source}\n[[action.constraints]]\nkind=\"param_nonempty\"\nparam=\"value\"\n");
        assert!(
            parse_automaton(&conflicting).is_err(),
            "a number cannot also be a nonempty string"
        );
        let conflict = format!(
            "{source}\n[[action.constraints]]\nkind=\"param_equals_field\"\nparam=\"value\"\nfield=\"Id\"\n"
        );
        assert!(
            parse_automaton(&conflict).is_err(),
            "identity and counter comparisons disagree"
        );
    }

    #[test]
    fn rejects_unsatisfiable_constraint_types() {
        for field_type in ["string", "bool", "list"] {
            assert!(
                parse_automaton(&spec(field_type, "\"value\"", "param_greater_than_field"))
                    .is_err()
            );
        }
        assert!(
            parse_automaton(&spec(
                "counter",
                r#"{name="value",type="string"}"#,
                "param_equals_field"
            ))
            .is_err()
        );
        assert!(parse_automaton(&spec("counter", "\"value\"", "param_greater_than_field")).is_ok());
        assert!(
            parse_automaton(&spec(
                "counter",
                r#"{name="value",type="int"}"#,
                "param_equals_field"
            ))
            .is_ok()
        );
    }

    #[test]
    fn rejects_numeric_defaults_outside_their_type() {
        for strict in ["true", "false"] {
            for (field_type, value) in [
                ("counter", "-1"),
                ("counter", "1.5"),
                ("counter", "\"0\""),
                ("int", "1.5"),
                ("int", "\"-3\""),
            ] {
                let source = spec(field_type, "\"value\"", "param_equals_field")
                    .replace(
                        "strict_action_params = true",
                        &format!("strict_action_params = {strict}"),
                    )
                    .replace("initial = 0", &format!("initial = {value}"));
                let uncontracted = source.split("[[action.constraints]]").next().unwrap();
                assert!(
                    parse_automaton(&source).is_err() && parse_automaton(uncontracted).is_err(),
                    "accepted {field_type} default {value}"
                );
            }
        }
    }

    #[test]
    fn rejects_comparison_types_without_a_scalar_runtime_contract() {
        for field_type in ["float", "number", "json", "object", "unknown"] {
            for kind in ["param_equals_field", "param_not_equals_field"] {
                let param = format!(r#"{{name="value",type="{field_type}"}}"#);
                let source = spec(field_type, &param, kind);
                assert!(
                    parse_automaton(&source).is_err(),
                    "accepted {kind} on {field_type}"
                );
            }
        }
    }

    #[test]
    fn accepts_supported_comparison_types() {
        for field_type in ["string", "bool", "int", "counter"] {
            let param = format!(r#"{{name="value",type="{field_type}"}}"#);
            for kind in ["param_equals_field", "param_not_equals_field"] {
                assert!(parse_automaton(&spec(field_type, &param, kind)).is_ok());
            }
        }
    }

    #[test]
    fn rejects_duplicate_action_contracts_before_last_rule_can_replace_first() {
        let source = spec("counter", "\"value\"", "param_equals_field");
        let duplicate = format!(
            "{source}\n[[action]]\nname = \"Observe\"\nfrom = [\"Ready\"]\nparams = [\"sequence\"]\n"
        );
        assert!(parse_automaton(&duplicate).is_err());
        assert!(
            parse_automaton(&duplicate.replace(
                "strict_action_params = true",
                "strict_action_params = false"
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_integer_defaults() {
        for value in ["\"not-an-integer\"", "1.5", "9223372036854775808"] {
            let source = spec("int", "\"value\"", "param_equals_field")
                .replace("initial = 0", &format!("initial = {value}"));
            assert!(
                parse_automaton(&source).is_err(),
                "accepted int default {value}"
            );
        }
        assert!(
            parse_automaton(
                &spec("int", "\"value\"", "param_equals_field")
                    .replace("initial = 0", "initial = -3")
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_constraint_references_without_a_runtime_value() {
        for name in [
            "has_spec",
            "HasSpec",
            "ctx_owner_status",
            "Status",
            "status",
        ] {
            let source = spec("counter", "\"value\"", "param_equals_field")
                .replace("field = \"sequence\"", &format!("field = \"{name}\""));
            assert!(
                parse_automaton(&source).is_err(),
                "accepted unresolved field {name}"
            );
        }
        for name in ["Id", "id"] {
            let source = spec("counter", "\"value\"", "param_equals_field")
                .replace("field = \"sequence\"", &format!("field = \"{name}\""));
            assert!(parse_automaton(&source).is_ok());
        }
    }
}
