//! The cascade's model check walks only for the invariants induction does
//! not prove, and says which it proved.

use temper_verify::{CascadeLevel, CascadeResult, LevelResult, VerificationCascade};

/// A job finished once, with `done` set as it finishes, and three flags
/// set one after another, each only once the one before it is.
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "done"
type = "bool"
initial = false

[[state]]
name = "a"
type = "bool"
initial = false

[[state]]
name = "b"
type = "bool"
initial = false

[[state]]
name = "c"
type = "bool"
initial = false

[[action]]
name = "Finish"
kind = "input"
from = ["Open"]
to = "Done"
effect = ["done = true"]

[[action]]
name = "SetA"
kind = "input"
from = ["Open"]
effect = ["a = true"]

[[action]]
name = "SetB"
kind = "input"
from = ["Open"]
guard = "a == true"
effect = ["b = true"]

[[action]]
name = "SetC"
kind = "input"
from = ["Open"]
guard = "b == true"
effect = ["c = true"]
"#;

fn run(name: &str, assert: &str) -> CascadeResult {
    let spec = format!("{JOB}\n[[invariant]]\nname = \"{name}\"\nassert = \"{assert}\"\n");
    VerificationCascade::from_ioa(&spec)
        .with_sim_seeds(1)
        .with_prop_test_cases(10)
        .run()
}

fn model_check(result: &CascadeResult) -> &LevelResult {
    result
        .levels
        .iter()
        .find(|level| level.level == CascadeLevel::ModelCheck)
        .expect("the model check ran")
}

/// The states a walk of every reachable state sees: both statuses, with
/// `a`, `b` and `c` set in order and `done` set in Done alone.
const EVERY_STATE: usize = 8;

#[test]
fn an_invariant_induction_proves_is_not_walked() {
    let result = run("DoneIsSet", "status == 'Done' => done == true");
    let l1 = model_check(&result);
    assert!(l1.passed, "{}", l1.summary);
    assert!(
        l1.summary.contains("DoneIsSet proved by induction"),
        "{}",
        l1.summary
    );
    // Only the search for moves that never fire ran, and it stopped once
    // every move had fired.
    let explored = l1
        .verification
        .as_ref()
        .expect("a model check")
        .states_explored;
    assert!(explored < EVERY_STATE, "{}", l1.summary);
}

#[test]
fn a_true_invariant_induction_does_not_prove_is_walked() {
    let result = run("CNeedsA", "c == true => a == true");
    let l1 = model_check(&result);
    assert!(l1.passed, "{}", l1.summary);
    assert!(
        !l1.summary.contains("proved by induction"),
        "{}",
        l1.summary
    );
    let explored = l1
        .verification
        .as_ref()
        .expect("a model check")
        .states_explored;
    assert_eq!(explored, EVERY_STATE, "{}", l1.summary);
}

#[test]
fn a_false_invariant_is_walked_to_a_counterexample_and_says_why_it_was_not_proved() {
    let result = run("DoneNeedsA", "status == 'Done' => a == true");
    let l1 = model_check(&result);
    assert!(!l1.passed, "{}", l1.summary);
    let verification = l1.verification.as_ref().expect("a model check");
    assert!(!verification.counterexamples.is_empty(), "{}", l1.summary);
    assert!(
        l1.diagnostics
            .iter()
            .any(|d| d.starts_with("DoneNeedsA not proved by induction: ")
                && d.contains("Job.Finish")),
        "{:?}",
        l1.diagnostics
    );
}

#[test]
fn a_proved_invariant_keeps_the_strings_a_remaining_liveness_rule_needs() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Open"]
initial = "Open"

[[state]]
name = "token"
type = "string"
initial = ""

[[action]]
name = "Work"
kind = "input"
from = ["Open"]
guard = "token == ''"

[[invariant]]
name = "TokenStaysEmpty"
assert = "token == ''"

[[liveness]]
name = "WorkIsEnabled"
from = ["Open"]
has_actions = true
"#;
    assert_model_preserved(spec, "TokenStaysEmpty", 1);
}

fn assert_model_preserved(spec: &str, rule: &str, states: usize) {
    let original = temper_verify::model::build_model_from_ioa(spec, 2).expect("fixture parses");
    let baseline = temper_verify::checker::check_model(&original);
    assert!(baseline.all_properties_hold, "{baseline:?}");
    assert_eq!(baseline.states_explored, states, "{baseline:?}");

    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(1)
        .with_prop_test_cases(10)
        .run();
    let l1 = model_check(&result);
    assert!(
        l1.summary.contains(&format!("{rule} proved by induction")),
        "{}",
        l1.summary
    );
    assert!(l1.passed, "{}; {:?}", l1.summary, l1.diagnostics);
    let verification = l1.verification.as_ref().expect("a model check");
    assert!(verification.is_complete, "{verification:?}");
    assert!(verification.counterexamples.is_empty(), "{verification:?}");
    assert!(verification.dead_transitions.is_empty(), "{verification:?}");
    assert_eq!(verification.states_explored, baseline.states_explored);
}

#[test]
fn a_proved_invariant_keeps_string_parameter_candidates() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Open"]
initial = "Open"

[[state]]
name = "token"
type = "string"
initial = ""

[[action]]
name = "Work"
kind = "input"
from = ["Open"]
params = [{ name = "token", type = "string" }]

[[invariant]]
name = "ReservedIsOpen"
assert = "token == 'reserved' => status == 'Open'"

[[liveness]]
name = "WorkIsEnabled"
from = ["Open"]
has_actions = true
"#;
    // The walk must still see empty, 'reserved' and a fresh token, even
    // though only the proved invariant mentions that literal.
    assert_model_preserved(spec, "ReservedIsOpen", 3);
}

#[test]
fn a_proved_invariant_keeps_list_parameter_candidates() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Open", "Done"]
initial = "Open"
terminal = ["Done"]

[[state]]
name = "tags"
type = "list"
initial = []

[[action]]
name = "Work"
kind = "input"
from = ["Open"]
to = "Done"
effect = ["append(tags, params.tag)"]

[[invariant]]
name = "ReservedHasStatus"
assert = "'reserved' in tags => status in ['Open', 'Done']"

[[liveness]]
name = "WorkIsEnabled"
from = ["Open"]
has_actions = true
"#;
    // 'reserved' and an unmentioned tag must both remain candidates.
    assert_model_preserved(spec, "ReservedHasStatus", 3);
}

#[test]
fn a_proof_keeps_an_unproved_assertion_with_the_same_name() {
    for (first, second) in [
        ("status == 'Open'", "ready == true"),
        ("ready == true", "status == 'Open'"),
    ] {
        let spec = format!(
            r#"
[automaton]
name = "Job"
states = ["Open"]
initial = "Open"

[[state]]
name = "ready"
type = "bool"
initial = false

[[action]]
name = "Wait"
kind = "input"
from = ["Open"]

[[invariant]]
name = "I"
assert = "{first}"

[[invariant]]
name = "I"
assert = "{second}"
"#
        );
        assert_duplicate_name_violation_is_walked(&spec);
    }
}

#[test]
fn a_proof_keeps_a_same_named_assertion_that_an_action_breaks() {
    for (first, second) in [
        ("status == 'Open'", "ready == false"),
        ("ready == false", "status == 'Open'"),
    ] {
        let spec = format!(
            r#"
[automaton]
name = "Job"
states = ["Open"]
initial = "Open"

[[state]]
name = "ready"
type = "bool"
initial = false

[[action]]
name = "Ready"
kind = "input"
from = ["Open"]
effect = ["ready = true"]

[[invariant]]
name = "I"
assert = "{first}"

[[invariant]]
name = "I"
assert = "{second}"
"#
        );
        assert_duplicate_name_violation_is_walked(&spec);
    }
}

fn assert_duplicate_name_violation_is_walked(spec: &str) {
    let original = temper_verify::model::build_model_from_ioa(spec, 2).expect("fixture parses");
    let baseline = temper_verify::checker::check_model(&original);
    assert!(!baseline.all_properties_hold, "{baseline:?}");
    assert!(!baseline.counterexamples.is_empty(), "{baseline:?}");

    let automaton = temper_spec::automaton::parse_automaton(spec).expect("fixture parses");
    let proofs = temper_verify::induct::prove_alone(&automaton, None);
    assert_eq!(proofs.len(), 2);
    assert_eq!(proofs.iter().filter(|proof| proof.proved()).count(), 1);

    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(1)
        .with_prop_test_cases(10)
        .run();
    let l1 = model_check(&result);
    assert!(
        l1.summary.contains("I proved by induction"),
        "{}",
        l1.summary
    );
    assert!(!l1.passed, "{}", l1.summary);
    let verification = l1.verification.as_ref().expect("a model check");
    assert!(!verification.counterexamples.is_empty(), "{verification:?}");
    assert_eq!(
        verification.all_properties_hold,
        baseline.all_properties_hold
    );
}
