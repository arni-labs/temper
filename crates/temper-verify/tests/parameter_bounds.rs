//! Numeric parameter domains follow the fields their effects use.

use stateright::Model;
use temper_verify::model::{ParamValue, build_model_from_ioa, semantics::successors};
use temper_verify::{CascadeLevel, TemperModelState, VerificationCascade, check_model};

const COUNTERS: &str = r#"
[automaton]
name = "Counters"
states = ["On"]
initial = "On"

[[state]]
name = "x"
type = "counter"
initial = 0

[[state]]
name = "y"
type = "counter"
initial = 0

[[state]]
name = "z"
type = "counter"
initial = 0

[[action]]
name = "Write"
from = ["On"]
guard = "x < 64"
effect = ["y = params.a", "z = params.b"]

[[action]]
name = "Bump"
from = ["On"]
effect = ["x += 1"]
"#;

#[test]
fn an_unrelated_threshold_does_not_expand_counter_parameters() {
    for op in ["=", "+=", "-="] {
        let spec = COUNTERS
            .replace("y = params.a", &format!("y {op} params.a"))
            .replace("z = params.b", &format!("z {op} params.b"));
        let model = build_model_from_ioa(&spec, 2).unwrap();
        let initial = model.init_states().remove(0);
        let choices = successors(&model, &model.transitions[0], &initial);
        assert_eq!(choices.len(), 9, "{op}: two independent domains of 0..=2");
        assert!(choices.iter().all(|(params, _)| {
            params
                .values()
                .all(|value| matches!(value, ParamValue::Count(0..=2)))
        }));
    }
}

#[test]
fn the_counter_regression_runs_the_cascade_and_keeps_bounded_reporting() {
    let result = VerificationCascade::from_ioa(COUNTERS)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(result.all_passed, "{result:?}");
    assert!(result.stopped.is_none(), "{result:?}");
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(
        l1.verification
            .as_ref()
            .unwrap()
            .bounded_by
            .contains(&("x".into(), 65))
    );
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
    assert!(l1.summary.contains("x up to 65"), "{}", l1.summary);
    assert!(
        !l1.summary.contains("all properties hold"),
        "{}",
        l1.summary
    );
}

const LIST: &str = r#"
[automaton]
name = "List"
states = ["On"]
initial = "On"

[[state]]
name = "x"
type = "counter"
initial = 0

[[state]]
name = "items"
type = "list"
initial = ["first", "second"]

[[action]]
name = "Remove"
from = ["On"]
guard = "x < 5000"
effect = ["remove_at(items, params.index)"]

[[action]]
name = "Append"
from = ["On"]
effect = ["append(items, 'new')"]
"#;

#[test]
fn an_unrelated_threshold_does_not_expand_list_indexes() {
    let model = build_model_from_ioa(LIST, 2).unwrap();
    let initial = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &initial);
    assert_eq!(choices.len(), 3, "two indexes and one out-of-range no-op");
    assert_eq!(choices[0].1.lists["items"], ["second"]);
    assert_eq!(choices[1].1.lists["items"], ["first"]);
    assert_eq!(choices[2].1.lists["items"], ["first", "second"]);

    let result = VerificationCascade::from_ioa(LIST)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    assert!(result.all_passed, "{result:?}");
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(
        l1.verification
            .as_ref()
            .unwrap()
            .bounded_by
            .contains(&("items".into(), 2))
    );
    assert!(l1.summary.contains("items up to 2"), "{}", l1.summary);
}

#[test]
fn one_parameter_covers_every_counter_its_effects_use() {
    for op in ["=", "+=", "-="] {
        let spec = COUNTERS.replace(
            "y = params.a\", \"z = params.b",
            &format!("y -= params.a\", \"x {op} params.a"),
        );
        let model = build_model_from_ioa(&spec, 2).unwrap();
        let mut initial = model.init_states().remove(0);
        initial
            .counters
            .insert("x".into(), if op == "-=" { 65 } else { 0 });
        let choices = successors(&model, &model.transitions[0], &initial);
        assert!(
            choices
                .iter()
                .any(|(params, _)| params["a"] == ParamValue::Count(65)),
            "{op}: {choices:?}"
        );
    }
}

#[test]
fn list_indexes_cover_an_initial_list_longer_than_the_default_bound() {
    let spec = LIST.replace(
        "initial = [\"first\", \"second\"]",
        "initial = [\"first\", \"second\", \"third\", \"fourth\"]",
    );
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let initial = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &initial);
    assert_eq!(choices.len(), 5, "four indexes and one out-of-range no-op");
    assert_eq!(choices[3].1.lists["items"], ["first", "second", "third"]);
    assert_eq!(choices[4].1.lists["items"], initial.lists["items"]);
}

/// Effects read the writes before them, even when the temporary is reset.
#[test]
fn a_parameter_reaches_a_later_counter_through_ordered_effects() {
    for (initial_x, operation, invariant, expected_x) in [
        (5, "-=", "x != 1", 1),
        (0, "=", "x <= 3", 4),
        (0, "+=", "x <= 3", 4),
    ] {
        let spec = format!(
            r#"
[automaton]
name = "Indirect"
states = ["On", "Done"]
initial = "On"

[[state]]
name = "x"
type = "counter"
initial = {initial_x}

[[state]]
name = "y"
type = "counter"
initial = 0

[[action]]
name = "Write"
from = ["On"]
to = "Done"
effect = ["y = params.a", "x {operation} y", "y = 0"]

[[invariant]]
name = "Safe"
assert = "{invariant}"
"#
        );
        let model = build_model_from_ioa(&spec, 2).unwrap();
        let initial = model.init_states().remove(0);
        let choices = successors(&model, &model.transitions[0], &initial);

        let result = VerificationCascade::from_ioa(&spec)
            .with_sim_seeds(2)
            .with_prop_test_cases(20)
            .run();
        let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
        assert!(!l1.passed, "{operation}: {}", l1.summary);
        let verification = l1.verification.as_ref().unwrap();
        let counterexample = verification
            .counterexamples
            .iter()
            .find(|c| c.violated.iter().any(|name| name == "Safe"))
            .expect("L1 finds the indirect parameter counterexample");
        let (last, _) = counterexample.trace.last().unwrap();
        assert_eq!(last.counters["x"], expected_x);
        assert_eq!(last.counters["y"], 0);
        assert!(
            counterexample.trace.iter().any(|(_, action)| {
                action
                    .as_ref()
                    .is_some_and(|a| a.params["a"] == ParamValue::Count(4))
            }),
            "{counterexample:?}"
        );
        assert!(
            choices.iter().any(|(params, next)| {
                params["a"] == ParamValue::Count(4)
                    && next.counters["x"] == expected_x
                    && next.counters["y"] == 0
            }),
            "{operation}: {choices:?}"
        );
    }
}

#[test]
fn an_overwrite_stops_a_parameter_from_reaching_unrelated_counters() {
    for effects in [
        "x = y\", \"y = params.a",
        "y = params.a\", \"y = 0\", \"x = y",
        "y = params.a\", \"y = params.b\", \"x = y",
    ] {
        let spec = COUNTERS.replace("y = params.a\", \"z = params.b", effects);
        let model = build_model_from_ioa(&spec, 2).unwrap();
        let initial = model.init_states().remove(0);
        let choices = successors(&model, &model.transitions[0], &initial);
        assert!(
            choices
                .iter()
                .all(|(params, _)| { matches!(params["a"], ParamValue::Count(0..=2)) }),
            "{effects}: {choices:?}"
        );
    }
}

#[test]
fn list_indexes_cover_appends_earlier_in_the_same_action() {
    let spec = LIST
        .split("\n[[action]]\nname = \"Append\"")
        .next()
        .unwrap()
        .replace("states = [\"On\"]", "states = [\"On\", \"Done\"]")
        .replace("guard = \"x < 5000\"", "to = \"Done\"")
        .replace(
            "remove_at(items, params.index)",
            "append(items, 'new')\", \"remove_at(items, params.index)\", \"remove_at(items, 0)",
        );
    let spec = format!(
        "{spec}\n[[invariant]]\nname = \"Safe\"\nassert = \"status == 'Done' => len(items) <= 1\"\n"
    );
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result
            .counterexamples
            .iter()
            .any(|c| c.violated.contains(&"Safe".into())),
        "{result:?}"
    );
    let initial = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &initial);
    assert!(
        choices.iter().any(|(params, next)| {
            params["index"] == ParamValue::Count(3) && next.lists["items"] == ["second", "new"]
        }),
        "{choices:?}"
    );
}

/// A move from `On` to `Done` with `effects`, over counters `x`, `y` and `w`
/// that start at 0 and `z` that starts at 5, and the invariant `assert`.
fn arithmetic(effects: &[&str], assert: &str) -> String {
    let effects = effects
        .iter()
        .map(|e| format!("\"{e}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let counter = |name: &str, initial: u32| {
        format!("[[state]]\nname = \"{name}\"\ntype = \"counter\"\ninitial = {initial}\n")
    };
    format!(
        r#"
[automaton]
name = "Arithmetic"
states = ["On", "Done"]
initial = "On"

{}
{}
{}
{}
[[action]]
name = "Write"
from = ["On"]
to = "Done"
effect = [{effects}]

[[invariant]]
name = "Safe"
assert = "{assert}"
"#,
        counter("x", 0),
        counter("y", 0),
        counter("w", 0),
        counter("z", 5),
    )
}

fn model_check(spec: &str) -> temper_verify::LevelResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run()
        .level_result(CascadeLevel::ModelCheck)
        .cloned()
        .expect("L1 ran")
}

/// The counterexample L1 found for `Safe`: the parameter `a` it chose, and
/// the last state.
fn broken_safe(l1: &temper_verify::LevelResult) -> (ParamValue, TemperModelState) {
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.violated.iter().any(|name| name == "Safe"))
        .unwrap_or_else(|| panic!("no counterexample for Safe: {}", l1.summary));
    let a = found
        .trace
        .iter()
        .find_map(|(_, action)| action.as_ref()?.params.get("a").cloned())
        .expect("the move's parameter");
    (a, found.trace.last().unwrap().0.clone())
}

/// A parameter is explored as far as its value can travel through the
/// move's arithmetic and still land inside the bounds: here `a` must reach
/// 4 for `x`, bounded at 2, to be 1.
#[test]
fn a_parameter_reaches_a_counter_through_a_subtraction() {
    let spec = arithmetic(&["y = params.a", "y -= 3", "x = y", "y = 0"], "x != 1");
    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(4));
    assert_eq!(last.counters["x"], 1);
}

#[test]
fn a_parameter_reaches_a_counter_through_an_addition() {
    let spec = arithmetic(&["y = params.a", "y += 2", "x = y", "y = 0"], "x != 4");
    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(2));
    assert_eq!(last.counters["x"], 4);
}

#[test]
fn a_parameter_reaches_a_counter_through_a_chain() {
    let spec = arithmetic(
        &[
            "y = params.a",
            "y -= 1",
            "w = y",
            "w -= 2",
            "x = w",
            "y = 0",
            "w = 0",
        ],
        "x != 2",
    );
    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(5));
    assert_eq!(last.counters["x"], 2);
}

/// A parameter can always take `x` past the bound it is explored to, so a
/// pass says it is bounded and names `x`.
#[test]
fn a_parameter_that_can_take_a_counter_past_its_bound_makes_a_bounded_pass() {
    let spec = arithmetic(&["y = params.a", "y -= 3", "x = y", "y = 0"], "y == 0");
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
    let cut = &l1.verification.as_ref().unwrap().bounded_by;
    assert!(cut.iter().any(|(name, _)| name == "x"), "{cut:?}");
}

/// What `params.b` can usefully be depends on `params.a`, which `x` took
/// earlier in the move, so its range is widened and the pass says so.
#[test]
fn a_subtraction_that_cannot_be_worked_back_makes_a_bounded_pass() {
    let spec = arithmetic(&["x = params.a", "x -= params.b"], "y == 0");
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    let cut = &l1.verification.as_ref().unwrap().bounded_by;
    assert!(cut.iter().any(|(name, _)| name == "params.b"), "{cut:?}");
}
