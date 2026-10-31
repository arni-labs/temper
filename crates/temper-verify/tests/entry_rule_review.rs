//! Regressions for entry-rule coverage and executable, correlated witnesses.

use stateright::Model;
use temper_verify::model::{ParamValue, build_model_from_ioa};
use temper_verify::{CascadeLevel, VerificationCascade, check_model};

fn spec(state: &str, action: &str, assertion: &str) -> String {
    format!(
        r#"
[automaton]
name = "Entry"
states = ["Waiting", "BeingMade"]
initial = "Waiting"
{state}
[[action]]
name = "Enter"
from = ["Waiting"]
to = "BeingMade"
{action}
[[entry_rule]]
name = "entry"
place = "BeingMade"
assert = "{assertion}"
"#
    )
}

fn string(name: &str, initial: &str) -> String {
    format!("\n[[state]]\nname = \"{name}\"\ntype = \"string\"\ninitial = \"{initial}\"\n")
}

fn list(name: &str, initial: &str) -> String {
    format!("\n[[state]]\nname = \"{name}\"\ntype = \"list\"\ninitial = {initial}\n")
}

fn passes(source: &str) {
    let result = VerificationCascade::from_ioa(source)
        .with_sim_seeds(2)
        .with_prop_test_cases(30)
        .run();
    assert!(result.all_passed, "{result:?}");
    for level in [
        CascadeLevel::ModelCheck,
        CascadeLevel::Simulation,
        CascadeLevel::PropertyTest,
    ] {
        assert!(result.level_result(level).unwrap().passed);
    }
}

#[test]
fn output_only_entry_coverage_fails_to_load() {
    let source = spec("", "kind = \"output\"", "false");
    let error = temper_spec::automaton::parse_automaton(&source)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no action moves into 'BeingMade' from a place it covers"),
        "{error}"
    );
    // Real input and internal transitions still provide coverage.
    for kind in ["input", "internal"] {
        temper_spec::automaton::parse_automaton(&source.replace("output", kind)).unwrap();
    }
}

fn constrained(initial: &str) -> String {
    spec(
        &string("expected", initial),
        r#"
params = [{ name = "p", type = "string" }]
[[action.constraints]]
kind = "param_equals_field"
param = "p"
field = "expected"
[[action.constraints]]
kind = "param_nonempty"
param = "p"
"#,
        "expected == 'Allowed'",
    )
}

#[test]
fn impossible_parameters_cannot_break_an_entry_rule() {
    for initial in ["", "   "] {
        let model = build_model_from_ioa(&constrained(initial), 2).unwrap();
        let result = check_model(&model);
        assert!(result.is_complete, "{result:?}");
        assert!(
            !result
                .counterexamples
                .iter()
                .any(|c| c.property == "EntryRules"),
            "{result:?}"
        );
        assert_eq!(result.dead_transitions, ["Enter [Waiting -> BeingMade]"]);
    }
}

#[test]
fn a_breaking_move_has_an_executable_parameter_assignment() {
    let model = build_model_from_ioa(&constrained("Stocked"), 2).unwrap();
    let result = check_model(&model);
    let found = result
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .unwrap();
    assert_eq!(found.violated, ["entry"]);
    let [(before, Some(action)), (after, None)] = found.trace.as_slice() else {
        panic!("missing breaking move: {:?}", found.trace);
    };
    assert_eq!(action.name, "Enter");
    assert_eq!(action.params["p"], ParamValue::Str("Stocked".into()));
    let mut actions = Vec::new();
    model.actions(before, &mut actions);
    assert!(actions.contains(action));
    assert_eq!(
        model.next_state(before, action.clone()).as_ref(),
        Some(after)
    );
}

#[test]
fn scalar_aliases_share_one_related_status() {
    passes(&spec(
        &(string("left", "same") + &string("right", "same")),
        "guard = \"Topping[left].status == 'Stocked'\"",
        "Topping[right].status == 'Stocked'",
    ));
}

#[test]
fn overlapping_lists_and_scalar_references_share_record_statuses() {
    for state in [
        list("left", "['one', 'two']") + &list("right", "['two', 'one', 'two']"),
        list("left", "['one', 'two']") + &string("right", "two"),
    ] {
        passes(&spec(
            &state,
            "guard = \"Topping[left].status == 'Stocked'\"",
            "Topping[right].status == 'Stocked'",
        ));
    }
}

#[test]
fn distinct_records_and_types_keep_independent_statuses() {
    for (right, entity) in [("different", "Topping"), ("same", "Other")] {
        let source = spec(
            &(string("left", "same") + &string("right", right)),
            "guard = \"Topping[left].status == 'Stocked'\"",
            &format!("{entity}[right].status == 'Stocked'"),
        );
        let result = check_model(&build_model_from_ioa(&source, 2).unwrap());
        assert!(
            result
                .counterexamples
                .iter()
                .any(|c| c.property == "EntryRules"),
            "{result:?}"
        );
    }
}

#[test]
fn guard_only_local_strings_keep_their_actual_values() {
    let source = spec(
        &(string("expected", "Stocked") + &string("topping", "one")),
        "guard = \"Topping[topping].status == expected\"",
        "Topping[topping].status == 'Stocked'",
    );
    passes(&source);
    passes(&format!(
        "{source}\n[[invariant]]\nname = \"read_expected\"\nassert = \"expected == 'Stocked'\"\n"
    ));
}

#[test]
fn guard_only_string_constraints_preserve_equalities_after_writes() {
    let source = spec(
        &(string("expected", "Stocked") + &string("topping", "one")),
        "guard = \"Topping[topping].status == expected && expected == 'Stocked'\"",
        "Topping[topping].status == 'Stocked'",
    );
    passes(&format!(
        r#"{source}
[[action]]
name = "Write"
from = ["Waiting"]
params = [{{ name = "expected", type = "string" }}]
"#
    ));
}

#[test]
fn impossible_parameters_are_checked_even_when_the_rule_reads_no_strings() {
    let source =
        constrained("").replace("assert = \"expected == 'Allowed'\"", "assert = \"false\"");
    let result = check_model(&build_model_from_ioa(&source, 2).unwrap());
    assert!(result.is_complete, "{result:?}");
    assert!(
        !result
            .counterexamples
            .iter()
            .any(|c| c.property == "EntryRules"),
        "{result:?}"
    );
    assert_eq!(result.dead_transitions, ["Enter [Waiting -> BeingMade]"]);
}

#[test]
fn symbolic_aliases_share_one_related_status_after_writes() {
    let source = spec(
        &(string("left", "") + &string("right", "")),
        "guard = \"left != '' && left == right && Topping[left].status == 'Stocked'\"",
        "Topping[right].status == 'Stocked'",
    );
    passes(&format!(
        r#"{source}
[[action]]
name = "Write"
from = ["Waiting"]
params = [{{ name = "left", type = "string" }}, {{ name = "right", type = "string" }}]
[[action.constraints]]
kind = "param_nonempty"
param = "left"
"#
    ));
}

#[test]
fn a_shared_record_cannot_satisfy_contradictory_guards_through_overlapping_lists() {
    let source = spec(
        &(list("left", "['one', 'shared']") + &list("right", "['shared', 'two']")),
        "guard = \"Topping[left].status == 'Stocked' && Topping[right].status != 'Stocked'\"",
        "false",
    );
    passes(&source);
    // Disjoint lists admit that guard, so the false assertion must break.
    let disjoint = source.replace("['shared', 'two']", "['three', 'two']");
    let result = check_model(&build_model_from_ioa(&disjoint, 2).unwrap());
    assert!(
        result
            .counterexamples
            .iter()
            .any(|c| c.property == "EntryRules"),
        "{result:?}"
    );
}
