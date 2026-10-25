use std::collections::BTreeMap;
use std::time::Duration;

use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A parent whose `value` its child copies through a reaction.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open"]
initial = "Open"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Set"
kind = "input"
from = ["Open"]
to = "Open"
params = [{ name = "value", type = "string" }]

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Child"
target_action = "Copy"

[action.triggers.args]
value = "value"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// The child, whose invariant says its copy is the parent's value. `bound`
/// ties Copy's parameter to the parent's field; without it anyone may send
/// Copy with any value.
fn child(bound: bool) -> String {
    let constraint = if bound {
        r#"
[[action.constraints]]
kind = "param_equals_related_field"
param = "value"
entity = "Parent"
id_field = "Id"
field = "value"
"#
    } else {
        ""
    };
    format!(
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
params = [{{ name = "value", type = "string" }}]
{constraint}
[[invariant]]
name = "copy_matches"
assert = "status == 'Done' => Parent[Id].value == value"
"#
    )
}

fn parse(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn budget() -> Budget {
    Budget {
        time: Duration::from_secs(30),
        states: 100_000,
    }
}

fn trace(entity: &str, action: &str, params: &[(&str, &str)]) -> TraceMove {
    TraceMove {
        entity: entity.to_string(),
        action: action.to_string(),
        params: params
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>(),
    }
}

#[test]
fn a_copy_anyone_may_write_breaks_the_invariant_with_the_move_that_wrote_it() {
    let child = child(false);
    let automatons = parse(&[PARENT, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let results = verify_joint_invariants(&refs, budget());
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(
        (result.entity.as_str(), result.invariant.as_str()),
        ("Child", "copy_matches")
    );
    assert_eq!(result.scope, ["Child", "Parent"]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert_eq!(
        path.len(),
        2,
        "start, then the one move that breaks it: {result}"
    );
    assert!(
        path[1]
            .action
            .as_deref()
            .is_some_and(|a| a.starts_with("Child.Copy")),
        "{result}"
    );
}

#[test]
fn a_copy_bound_to_the_related_field_holds() {
    let child = child(true);
    let automatons = parse(&[PARENT, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let results = verify_joint_invariants(&refs, budget());
    assert_eq!(results.len(), 1);
    assert!(results[0].holds(), "{}", results[0]);
}

#[test]
fn an_invariant_naming_an_undeclared_type_is_not_checked_and_says_why() {
    let child = child(true);
    let automatons = parse(&[&child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let results = verify_joint_invariants(&refs, budget());
    let JointOutcome::Incomplete { reason } = &results[0].outcome else {
        panic!("expected not checked, got {}", results[0]);
    };
    assert!(reason.contains("Parent"), "{reason}");
    assert!(!results[0].holds());
}

#[test]
fn specs_without_cross_entity_invariants_have_nothing_to_check() {
    let automatons = parse(&[PARENT]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    assert!(verify_joint_invariants(&refs, budget()).is_empty());
}

#[test]
fn a_replayed_trace_follows_the_reaction_and_names_what_it_breaks() {
    let bound = child(true);
    let automatons = parse(&[PARENT, &bound]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let replay =
        replay_joint_invariants(&refs, "Child", &[trace("Parent", "Set", &[("value", "A")])])
            .expect("Set is allowed");
    assert!(replay.broken.is_empty());
    assert!(
        replay.steps.last().unwrap().state.contains("Child=Done"),
        "the reaction copied it"
    );

    let refused =
        replay_joint_invariants(&refs, "Child", &[trace("Child", "Copy", &[("value", "B")])]);
    assert!(
        refused.is_err(),
        "a copy of a value the parent does not hold is refused"
    );

    let free = child(false);
    let automatons = parse(&[PARENT, &free]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let replay =
        replay_joint_invariants(&refs, "Child", &[trace("Child", "Copy", &[("value", "B")])])
            .expect("an unbound Copy is allowed");
    assert_eq!(replay.broken, ["copy_matches"]);
}
