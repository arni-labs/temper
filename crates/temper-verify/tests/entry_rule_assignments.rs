//! Entry assertions load through the parser; witnesses use the move's inputs.

use stateright::Model;
use temper_verify::model::{ParamValue, build_model_from_ioa};
use temper_verify::proptest_gen::run_prop_tests_on_model;
use temper_verify::simulation::{SimConfig, run_multi_seed_simulation};
use temper_verify::{VerificationCascade, check_model};

fn spec(state: &str, action: &str, assertion: &str) -> String {
    format!(
        r#"
[automaton]
name = "Entry"
states = ["Waiting", "Inside"]
initial = "Waiting"
{state}
[[action]]
name = "Enter"
from = ["Waiting"]
to = "Inside"
{action}
[[entry_rule]]
name = "entry"
place = "Inside"
assert = "{assertion}"
"#
    )
}

fn string(name: &str, initial: &str) -> String {
    format!("\n[[state]]\nname = \"{name}\"\ntype = \"string\"\ninitial = \"{initial}\"\n")
}

fn passes(source: &str) {
    let result = VerificationCascade::from_ioa(source)
        .with_sim_seeds(5)
        .with_prop_test_cases(100)
        .run();
    assert!(result.all_passed, "{result:?}");
}

fn refuses(source: &str) {
    let model = build_model_from_ioa(source, 2).expect("modeled assertions must load");
    let checked = check_model(&model);
    assert!(
        checked
            .counterexamples
            .iter()
            .any(|c| c.property == "EntryRules"),
        "{checked:?}"
    );
    let simulated = run_multi_seed_simulation(&model, &SimConfig::default(), 20);
    assert!(
        simulated
            .iter()
            .any(|s| s.violations.iter().any(|v| v.invariant == "entry")),
        "{simulated:?}"
    );
    let tested = run_prop_tests_on_model(&model, 1000, 10);
    assert_eq!(
        tested.failure.as_ref().map(|f| f.invariant.as_str()),
        Some("entry"),
        "{tested:?}"
    );
}

#[test]
fn nullable_related_status_assertions_load_and_verify() {
    let state = string("topping", "one");
    let assertion = "Topping[topping].status != null";
    passes(&spec(
        &state,
        &format!(r#"guard = "{assertion}""#),
        assertion,
    ));
    refuses(&spec(
        &state,
        r#"guard = "Topping[topping].status == null""#,
        assertion,
    ));
}

#[test]
fn local_membership_assertions_load_and_verify() {
    let state = string("expected", "Stocked")
        + "\n[[state]]\nname = \"allowed\"\ntype = \"list\"\ninitial = ['Stocked']\n";
    passes(&spec(&state, "", "expected in allowed"));
    refuses(&spec(
        &state.replace(r#"initial = "Stocked""#, r#"initial = "Other""#),
        "",
        "expected in allowed",
    ));
}

fn parameter_guard(guard: &str, assertion: &str) -> String {
    let source = spec(
        &(string("expected", "Other") + &string("topping", "one")),
        &format!(
            r#"
guard = "{guard}"
params = [{{ name = "p", type = "string" }}]
[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "expected"
"#
        ),
        assertion,
    );
    format!(
        r#"{source}
[[action]]
name = "Write"
from = ["Waiting"]
params = [{{ name = "expected", type = "string" }}]
"#
    )
}

#[test]
fn constrained_parameters_and_guard_share_the_entry_assignment() {
    passes(&parameter_guard(
        "params.p == 'Stocked'",
        "expected == 'Stocked'",
    ));
    passes(&parameter_guard(
        "params.p == 'Stocked' && Topping[topping].status == params.p",
        "Topping[topping].status == 'Stocked'",
    ));
}

#[test]
fn breaking_witness_uses_the_guard_satisfying_parameter() {
    let source = parameter_guard("params.p == 'Stocked'", "false");
    refuses(&source);
    let model = build_model_from_ioa(&source, 2).unwrap();
    let checked = check_model(&model);
    let found = checked
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .unwrap();
    assert_eq!(found.trace[0].1.as_ref().unwrap().name, "Write");
    let (before, Some(action)) = &found.trace[found.trace.len() - 2] else {
        panic!("missing entry move")
    };
    assert_eq!(action.name, "Enter");
    assert_eq!(before.strings["expected"], "Stocked".into());
    assert_eq!(action.params["p"], ParamValue::Str("Stocked".into()));
}

#[test]
fn guard_only_parameters_are_explored_and_correlated_with_statuses() {
    let source = spec(
        &string("topping", "one"),
        r#"params = [{ name = "p", type = "string" }]
guard = "params.p == 'Stocked' && Topping[topping].status == params.p""#,
        "Topping[topping].status == 'Stocked'",
    );
    passes(&source);
    refuses(&source.replace(
        r#"assert = "Topping[topping].status == 'Stocked'""#,
        r#"assert = "false""#,
    ));
}

#[test]
fn each_taken_parameter_assignment_is_checked_separately() {
    let source = spec(
        &string("topping", "one"),
        r#"params = [{ name = "p", type = "string" }]
guard = "(params.p == 'Stocked' && Topping[topping].status == 'Stocked') || (params.p == 'Other' && Topping[topping].status == 'Empty')""#,
        "Topping[topping].status == 'Stocked'",
    );
    refuses(&source);
    let model = build_model_from_ioa(&source, 2).unwrap();
    let checked = check_model(&model);
    let found = checked
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .unwrap();
    assert_eq!(
        found.trace[0].1.as_ref().unwrap().params["p"],
        ParamValue::Str("Other".into())
    );
    let tested = run_prop_tests_on_model(&model, 1000, 10);
    let trace = &tested.failure.unwrap().trace;
    assert_eq!(
        trace[trace.len() - 2].1.as_ref().unwrap().params["p"],
        ParamValue::Str("Other".into())
    );
    // Both legal inputs must be offered, including the safe one.
    let mut actions = Vec::new();
    model.actions(&model.init_states()[0], &mut actions);
    for value in ["Stocked", "Other"] {
        assert!(
            actions
                .iter()
                .any(|a| a.params["p"] == ParamValue::Str(value.into())),
            "{actions:?}"
        );
    }
}

#[test]
fn scalar_guard_inputs_obey_the_same_constraints() {
    for (kind, initial, required, breaking) in [
        ("bool", "false", "true", "false"),
        ("counter", "0", "50", "0"),
    ] {
        let state =
            format!("\n[[state]]\nname = \"expected\"\ntype = \"{kind}\"\ninitial = {initial}\n");
        let action = format!(
            r#"
params = [{{ name = "p", type = "{kind}" }}]
guard = "params.p == {required}"
[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "expected"
"#
        );
        let source = spec(&state, &action, &format!("expected == {required}"));
        let source = format!(
            r#"{source}
[[action]]
name = "Write"
from = ["Waiting"]
effect = ["expected = {required}"]
"#
        );
        passes(&source);
        refuses(&source.replace(
            &format!("assert = \"expected == {required}\""),
            &format!("assert = \"expected == {breaking}\""),
        ));
        // No effect reads this parameter; its guard alone must still give
        // it choices beyond the model's default count bound of two.
        refuses(&spec(
            &state,
            &format!(
                "params = [{{ name = \"p\", type = \"{kind}\" }}]\nguard = \"params.p == {required}\""
            ),
            "false",
        ));
    }
}

#[test]
fn a_guard_failing_prefix_cannot_create_an_entry_counterexample() {
    let source = parameter_guard("true", "expected == 'Stocked'")
        .replace(
            "states = [\"Waiting\", \"Inside\"]",
            "states = [\"Setup\", \"Waiting\", \"Inside\"]",
        )
        .replace("initial = \"Waiting\"", "initial = \"Setup\"")
        .replace(
            "name = \"Write\"\nfrom = [\"Waiting\"]",
            "name = \"Write\"\nfrom = [\"Setup\"]",
        );
    // Enter is unconditional; Arm is the gate before its source place.
    let source = source.replace("guard = \"true\"\nparams", "params");
    let source = format!(
        r#"{source}
[[action]]
name = "Arm"
from = ["Setup"]
to = "Waiting"
params = [{{ name = "p", type = "string" }}]
guard = "params.p == 'Stocked'"
[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "expected"
"#
    );
    passes(&source);
}
