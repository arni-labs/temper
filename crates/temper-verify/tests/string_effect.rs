use temper_verify::checker::check_model;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{CascadeLevel, VerificationCascade};

const SPEC: &str = include_str!("../../../test-fixtures/specs/string_effect.ioa.toml");

#[test]
fn string_clear_passes_every_cascade_level() {
    let result = VerificationCascade::from_ioa(SPEC)
        .with_sim_seeds(20)
        .with_prop_test_cases(100)
        .run();
    assert!(result.all_passed, "{result:#?}");
    for level in [
        CascadeLevel::SymbolicVerification,
        CascadeLevel::ModelCheck,
        CascadeLevel::Simulation,
        CascadeLevel::PropertyTest,
    ] {
        assert!(result.level_result(level).unwrap().passed);
    }
}

#[test]
fn invariant_needs_the_string_clear() {
    for broken in [
        SPEC.replace("\"error = ''\", ", ""),
        SPEC.replace("error = ''", "error = 'stale'"),
    ] {
        let result = check_model(&build_model_from_ioa(&broken, 2).unwrap());
        assert!(!result.all_properties_hold);
        assert!(
            result
                .counterexamples
                .iter()
                .any(|c| c.property == "Invariants"
                    && c.trace
                        .last()
                        .is_some_and(|(state, _)| !state.strings["error"].is_empty())),
            "{result:#?}"
        );
    }
}

#[test]
fn effect_only_strings_have_only_initial_and_literal_values_and_guards_see_writes() {
    let source = r#"
[automaton]
name = "FiniteStrings"
states = ["Running"]
initial = "Running"
allow_indefinite_states = ["Running"]

[[state]]
name = "message"
type = "string"
initial = "initial"

[[action]]
name = "Write"
from = ["Running"]
to = "Running"
effect = ["message = 'intermediate'", "message = 'written'"]

[[action]]
name = "Clear"
from = ["Running"]
to = "Running"
guard = "message == 'written'"
effect = ["message = ''"]
"#;
    let model = build_model_from_ioa(source, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result.all_properties_hold && result.is_complete,
        "{result:#?}"
    );
    assert!(result.dead_transitions.is_empty(), "{result:#?}");
    // Initial, final literal, empty. The intermediate write is never a state,
    // and effect-only fields acquire no arbitrary parameter equality symbols.
    assert_eq!(result.states_explored, 3);

    let broken = source.replace("message == 'written'", "message == 'intermediate'");
    let result = check_model(&build_model_from_ioa(&broken, 2).unwrap());
    assert_eq!(result.dead_transitions, ["Clear [Running -> Running]"]);
}

#[test]
fn smt_observes_the_literal_and_rejects_a_stale_assignment() {
    let broken = SPEC.replace("error = ''", "error = 'stale'");
    let result = temper_verify::smt::verify_symbolic(&broken, 2);
    assert!(!result.all_passed, "{result:#?}");
    assert!(
        result
            .inductive_invariants
            .iter()
            .any(|(name, passed)| name == "RecoveredClearsError" && !passed)
    );
}

#[test]
fn a_parameter_can_equal_a_literal_written_by_a_later_effect() {
    let source = r#"
[automaton]
name = "FutureLiteral"
states = ["Start", "Input", "Written", "Equal"]
initial = "Start"
terminal = ["Equal"]
[[state]]
name = "input"
type = "string"
initial = ""
[[state]]
name = "literal"
type = "string"
initial = ""
[[action]]
name = "Input"
from = ["Start"]
to = "Input"
params = ["input"]
[[action]]
name = "Write"
from = ["Input"]
to = "Written"
effect = ["literal = 'future'"]
[[action]]
name = "Compare"
from = ["Written"]
to = "Equal"
guard = "input == literal"
[[invariant]]
name = "NeverEqual"
assert = "status != 'Equal'"
"#;
    let result = check_model(&build_model_from_ioa(source, 2).unwrap());
    assert!(!result.all_properties_hold);
    let trace = &result.counterexamples[0].trace;
    let last = &trace.last().unwrap().0;
    assert_eq!(
        last.strings["input"],
        temper_verify::model::ModelString::from("future")
    );
}

#[test]
fn string_effects_keep_existing_parameter_constraints_in_the_cascade() {
    let source = SPEC.replace(
        "params = [\"error\"]",
        r#"params = ["error"]
[[action.constraints]]
kind = "param_nonempty"
param = "error"
[[action.constraints]]
kind = "param_equals_field"
param = "error"
field = "reason"
"#,
    ) + r#"
[[invariant]]
name = "FailureExplained"
assert = "status in ['Failed'] => !empty(error) && error == reason"
"#;
    let result = VerificationCascade::from_ioa(&source)
        .with_sim_seeds(10)
        .with_prop_test_cases(50)
        .run();
    assert!(result.all_passed, "{result:#?}");
}

#[test]
fn string_parameters_override_effects_in_every_cascade_level() {
    let source = include_str!("../../../test-fixtures/specs/string_effect_param_order.ioa.toml");
    let result = VerificationCascade::from_ioa(source)
        .with_sim_seeds(20)
        .with_prop_test_cases(100)
        .run();
    assert!(result.all_passed, "{result:#?}");
    for level in [
        CascadeLevel::SymbolicVerification,
        CascadeLevel::ModelCheck,
        CascadeLevel::Simulation,
        CascadeLevel::PropertyTest,
    ] {
        assert!(result.level_result(level).unwrap().passed);
    }
}
