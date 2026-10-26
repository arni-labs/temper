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
