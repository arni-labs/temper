//! Route proofs must honor the local inputs needed by each exit.

use super::{TOKEN, broken, model_check};

/// An unmodeled local integer must not act as a freely satisfiable route.
#[test]
fn an_unmodeled_integer_cannot_certify_a_route() {
    let spec = TOKEN
        .replace(
            "type = \"string\"\ninitial = \"\"",
            "type = \"int\"\ninitial = 0",
        )
        .replace("token == 'ok'", "token == 1");
    let l1 = model_check(&spec);
    assert!(!l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("not proved"), "{}", l1.summary);
    assert!(l1.summary.contains("token"), "{}", l1.summary);
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete, "the graph walk completed");
    assert!(
        result.counterexamples.is_empty(),
        "no concrete refusal was proved"
    );
    assert!(result.bounded_by.is_empty());
    assert_eq!(result.unproved_can_reach.len(), 1);
    assert!(result.unproved_can_reach[0].contains("can_finish"));
    // Without an author-stated route rule, numeric abstraction keeps its
    // existing behavior.
    let without_rule = spec.split("[[liveness]]").next().unwrap();
    let l1 = model_check(without_rule);
    assert!(l1.passed, "{}", l1.summary);
}

/// Constraints decide whether an exit exists even when its input does not
/// write a tracked variable and no guard or invariant reads the source.
#[test]
fn constraints_on_an_untracked_parameter_cannot_certify_an_impossible_exit() {
    let spec = TOKEN.replace(
        "guard = \"token == 'ok'\"",
        r#"
params = [{ name = "proof", type = "string" }]
[[action.constraints]]
kind = "param_nonempty"
param = "proof"
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
"#,
    );
    let l1 = model_check(&spec);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert!(moves.is_empty());
    assert_eq!(place, "Waiting");
    assert!(l1.verification.as_ref().unwrap().is_complete);
    let enabled = spec.replace("initial = \"\"", "initial = \"ok\"");
    let l1 = model_check(&enabled);
    assert!(l1.passed, "{}", l1.summary);
}

/// A parameter used for a string field and a list must keep the literals
/// required by both, including when the string value comes through a constraint.
#[test]
fn a_shared_string_and_list_parameter_keeps_list_choices() {
    let spec = format!(
        "{}{}",
        TOKEN.replace(
            "guard = \"token == 'ok'\"",
            "guard = \"!empty(token) && 'vip' in tags\""
        ),
        r#"
[[state]]
name = "tags"
type = "list"
initial = []
[[action]]
name = "Supply"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "token", type = "string" }]
effect = ["append(tags, params.token)"]
"#
    );
    // Exercise the model's actual input choice and resulting enabled exit.
    let model = temper_verify::build_model_from_ioa(&spec, 2).unwrap();
    use stateright::Model;
    use temper_verify::model::semantics::{successors, truth};
    let initial = model.init_states().remove(0);
    let supply = model
        .transitions
        .iter()
        .find(|t| t.name == "Supply")
        .unwrap();
    let (_, supplied) = successors(&model, supply, &initial)
        .into_iter()
        .find(|(params, _)| params["token"] == temper_verify::ParamValue::Str("vip".into()))
        .expect("the actual vip input is explored");
    assert_eq!(supplied.lists["tags"], ["vip"]);
    assert_eq!(supplied.strings["token"], "vip".into());
    let finish = model
        .transitions
        .iter()
        .find(|t| t.name == "Finish")
        .unwrap();
    assert!(truth(&finish.guard, &model.var_kinds, &supplied).must_hold());
    let result = temper_verify::check_model(&model);
    assert!(result.all_properties_hold, "{result:?}");
    assert!(result.dead_transitions.is_empty());
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.verification.as_ref().unwrap().counterexamples.is_empty());

    // The same choice must reach a source field, then be copied to a list
    // by an input which does not itself write a tracked field.
    let constrained = spec.replace("params = [{ name = \"token\", type = \"string\" }]\neffect = [\"append(tags, params.token)\"]", r#"
params = [{ name = "proof", type = "string" }]
effect = ["append(tags, params.proof)"]
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"

[[action]]
name = "SupplyToken"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "token", type = "string" }]
"#);
    assert_ne!(constrained, spec);
    let l1 = model_check(&constrained);
    assert!(l1.passed, "{}", l1.summary);
}
