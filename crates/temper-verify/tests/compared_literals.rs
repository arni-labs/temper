//! A counter is explored at least one past every literal a guard or
//! invariant compares it with.
//!
//! The model check explores each counter up to a bound. A rule whose
//! threshold sits above that bound was never reached: a count that passes
//! it was reported as holding, and a move that waits for it as dead.

use proptest::prelude::*;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{CascadeLevel, VerificationCascade, check_model};

/// A batch's age counts up by the hour and must stay under 4.
const COUNT_UP: &str = r#"
[automaton]
name = "PearlBatch"
states = ["Fresh", "Served"]
initial = "Fresh"

[[state]]
name = "age_hours"
type = "counter"
initial = 0

[[action]]
name = "HourPasses"
from = ["Fresh"]
effect = ["age_hours += 1"]

[[action]]
name = "Serve"
from = ["Fresh"]
to = "Served"

[[invariant]]
name = "NoStalePearls"
assert = "age_hours < 4"
"#;

/// A card earns its reward at its fifth stamp.
const PUNCH_CARD: &str = r#"
[automaton]
name = "Punchcard"
states = ["Collecting", "Rewarded"]
initial = "Collecting"

[[state]]
name = "stamps"
type = "counter"
initial = 0

[[action]]
name = "Stamp"
from = ["Collecting"]
effect = ["stamps += 1"]

[[action]]
name = "Reward"
from = ["Collecting"]
to = "Rewarded"
guard = "stamps >= 5"
"#;

fn cascade(spec: &str) -> temper_verify::CascadeResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run()
}

#[test]
fn a_count_that_passes_its_threshold_breaks_the_rule() {
    let result = cascade(COUNT_UP);
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .expect("L1 result")
        .counterexamples
        .iter()
        .find(|c| c.violated.iter().any(|name| name == "NoStalePearls"))
        .expect("a counterexample for NoStalePearls");
    let (last, _) = found.trace.last().expect("a trace");
    assert_eq!(last.counters.get("age_hours"), Some(&4), "{found:?}");
}

#[test]
fn a_move_that_waits_for_a_count_above_the_old_bound_can_fire() {
    let result = cascade(PUNCH_CARD);
    let l0 = result
        .level_result(CascadeLevel::SymbolicVerification)
        .expect("L0 ran");
    assert!(l0.passed, "{}", l0.summary);
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(l1.passed, "{}", l1.summary);
    // Stamps can go past any bound, so L1 says how far it looked.
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
    assert!(
        !l1.summary.contains("all properties hold"),
        "{}",
        l1.summary
    );
}

/// One counter `x` that one move adds 1 to, and an invariant `assert`,
/// with `guard` on the move when it is given.
fn counter_spec(guard: Option<&str>, assert: &str) -> String {
    let guard = guard
        .map(|g| format!("guard = \"{g}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
[automaton]
name = "Tally"
states = ["Open"]
initial = "Open"

[[state]]
name = "x"
type = "counter"
initial = 0

[[action]]
name = "Add"
from = ["Open"]
{guard}effect = ["x += 1"]

[[invariant]]
name = "UnderThreshold"
assert = "{assert}"
"#
    )
}

/// `x` compared with `threshold`, written each way round, and the first
/// value of `x` that breaks it.
fn comparison() -> impl Strategy<Value = (String, usize)> {
    (3usize..=24, 0usize..5).prop_map(|(t, form)| match form {
        0 => (format!("x < {t}"), t),
        1 => (format!("{t} > x"), t),
        2 => (format!("x <= {t}"), t + 1),
        3 => (format!("{t} >= x"), t + 1),
        _ => {
            let allowed: Vec<String> = (0..t).map(|n| n.to_string()).collect();
            (format!("x in [{}]", allowed.join(", ")), t)
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A threshold above the default bound of 2 is still reached: the model
    /// check breaks the rule at the first value past it.
    #[test]
    fn a_threshold_above_the_default_bound_is_reached((assert, breaks_at) in comparison()) {
        let spec = counter_spec(None, &assert);
        let result = check_model(&build_model_from_ioa(&spec, 2).unwrap());
        let found = result
            .counterexamples
            .iter()
            .find(|c| c.property == "Invariants");
        prop_assert!(found.is_some(), "{assert} held: {result:?}");
        let (last, _) = found.unwrap().trace.last().unwrap();
        prop_assert_eq!(last.counters.get("x"), Some(&breaks_at));
    }

    /// A count the guard keeps at its threshold is checked at every value it
    /// can take, and the result is a proof, not a bounded pass.
    #[test]
    fn a_count_held_at_its_threshold_is_proved(threshold in 3usize..=24) {
        let spec = counter_spec(Some(&format!("x < {threshold}")), &format!("x <= {threshold}"));
        let result = cascade(&spec);
        let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
        prop_assert!(l1.passed, "{}", l1.summary);
        prop_assert!(l1.summary.contains("all properties hold"), "{}", l1.summary);
        let explored = l1.verification.as_ref().unwrap().states_explored;
        prop_assert!(explored > threshold, "{explored} states for x up to {threshold}");
    }
}
