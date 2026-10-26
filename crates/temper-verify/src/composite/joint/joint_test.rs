use std::collections::BTreeMap;
use std::time::Duration;

use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A parent whose `value` its child copies through a reaction, and which a
/// child may finish only while the parent is Closed. `reopens` lets a Closed
/// parent open again.
fn parent(reopens: bool) -> String {
    let reopen = if reopens {
        r#"
[[action]]
name = "Reopen"
kind = "input"
from = ["Closed"]
to = "Open"
"#
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
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
params = [{{ name = "value", type = "string" }}]

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Child"
target_action = "Copy"

[action.triggers.args]
value = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
{reopen}
"#
    )
}

/// The child: `Copy` takes any value anyone sends; `Finish` needs its parent
/// Closed.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done", "Finished"]
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
params = [{ name = "value", type = "string" }]

[[action]]
name = "Finish"
kind = "input"
from = ["Waiting"]
to = "Finished"
guard = "Parent[Id].status == 'Closed'"
"#;

fn child_with(invariant: &str) -> String {
    format!("{CHILD}\n[[invariant]]\nname = \"rule\"\nassert = \"{invariant}\"\n")
}

fn parse(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn check(sources: &[&str]) -> JointInvariantResult {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let budget = Budget {
        time: Duration::from_secs(30),
        states: 100_000,
    };
    let mut results = verify_joint_invariants(&refs, budget);
    assert_eq!(results.len(), 1);
    results.remove(0)
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

fn ids(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(entity, id)| (entity.to_string(), id.to_string()))
        .collect()
}

#[test]
fn a_copy_anyone_may_write_breaks_the_invariant_with_the_move_that_wrote_it() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent(false);
    let result = check(&[&parent, &child]);
    assert_eq!(
        (result.entity.as_str(), result.invariant.as_str()),
        ("Child", "rule")
    );
    assert_eq!(result.scope, ["Child", "Parent"]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert_eq!(
        path.len(),
        2,
        "start, then the move that breaks it: {result}"
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
fn a_guard_on_a_related_status_that_cannot_change_back_holds() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let parent = parent(false);
    let result = check(&[&parent, &child]);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_related_status_read_on_the_dispatch_path_can_change_before_the_write() {
    let child = child_with("status == 'Finished' => Parent[Id].status == 'Closed'");
    let parent = parent(true);
    let result = check(&[&parent, &child]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    let moves: Vec<&str> = path.iter().filter_map(|s| s.action.as_deref()).collect();
    let sent = moves
        .iter()
        .position(|m| m.starts_with("Child.Finish") && m.ends_with("sent"));
    let reopened = moves.iter().position(|m| m.starts_with("Parent.Reopen"));
    let arrived = moves
        .iter()
        .position(|m| m.starts_with("Child.Finish") && m.contains("arrives"));
    assert!(
        matches!((sent, reopened, arrived), (Some(s), Some(r), Some(a)) if s < r && r < a),
        "Finish is sent while the parent is Closed, the parent reopens, then Finish lands: {result}"
    );
}

#[test]
fn an_invariant_naming_an_undeclared_type_is_not_checked_and_says_why() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let result = check(&[&child]);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("expected not checked, got {result}");
    };
    assert!(reason.contains("Parent"), "{reason}");
    assert!(!result.holds());
}

#[test]
fn specs_without_cross_entity_invariants_have_nothing_to_check() {
    let parent = parent(false);
    let automatons = parse(&[&parent, CHILD]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let budget = Budget {
        time: Duration::from_secs(30),
        states: 100_000,
    };
    assert!(verify_joint_invariants(&refs, budget).is_empty());
}

#[test]
fn a_replayed_reaction_is_in_flight_until_the_trace_takes_it() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent(false);
    let automatons = parse(&[&parent, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let same = ids(&[("Parent", "p"), ("Child", "p")]);

    let set = replay_joint_invariants(
        &refs,
        "Child",
        &same,
        &[trace("Parent", "Set", &[("value", "A")])],
    )
    .expect("Set is allowed");
    let last = &set.steps.last().unwrap().state;
    assert!(last.contains("in flight: Child.Copy"), "{last}");

    let copied = replay_joint_invariants(
        &refs,
        "Child",
        &same,
        &[
            trace("Parent", "Set", &[("value", "A")]),
            trace("Child", "Copy", &[("value", "A")]),
        ],
    )
    .expect("the trace takes the reaction");
    assert!(copied.broken.is_empty());
    let last = &copied.steps.last().unwrap().state;
    assert!(
        last.contains("Child=Done") && !last.contains("in flight"),
        "{last}"
    );

    let forged = replay_joint_invariants(
        &refs,
        "Child",
        &same,
        &[trace("Child", "Copy", &[("value", "B")])],
    )
    .expect("anyone may send Copy");
    assert_eq!(forged.broken, ["rule"]);
}

#[test]
fn a_same_id_reaction_reaches_no_record_with_another_id() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent(false);
    let automatons = parse(&[&parent, &child]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let apart = ids(&[("Parent", "p"), ("Child", "c")]);
    let set = replay_joint_invariants(
        &refs,
        "Child",
        &apart,
        &[trace("Parent", "Set", &[("value", "A")])],
    )
    .expect("Set is allowed");
    let last = &set.steps.last().unwrap().state;
    assert!(
        !last.contains("in flight"),
        "the reaction found no child: {last}"
    );
}

#[test]
fn unknown_related_reads_cannot_be_proven() {
    for assertion in [
        "Parent[Id].score > 0",
        "Parent[Id].absent == 'x'",
        "Parent[absent].status == 'Closed'",
    ] {
        let parent = "[automaton]\nname = 'Parent'\nstates = ['Open']\ninitial = 'Open'\n[[state]]\nname = 'score'\ntype = 'int'\ninitial = 1\n";
        let child = child_with(assertion);
        let result = check(&[parent, &child]);
        assert!(
            matches!(result.outcome, JointOutcome::Incomplete { .. }),
            "{result}"
        );
    }
}

#[test]
fn a_literal_in_a_related_read_is_explored_by_the_parent_setter() {
    let child = child_with("Parent[Id].value != 'bad'");
    let result = check(&[&parent(false), &child]);
    let JointOutcome::Broken { path } = result.outcome else {
        panic!("{result}");
    };
    assert!(path.iter().any(|s| s.state.contains("value='bad'")));
}

#[test]
fn initial_partitions_stop_within_a_tiny_budget() {
    let sources: Vec<String> = (0..13)
        .map(|i| format!("[automaton]\nname = 'T{i}'\nstates = ['Ready']\ninitial = 'Ready'\n"))
        .collect();
    let assertion = (1..13)
        .map(|i| format!("T{i}[Id].status != 'Never'"))
        .collect::<Vec<_>>()
        .join(" && ");
    let mut sources = sources;
    sources[0].push_str(&format!(
        "\n[[invariant]]\nname = 'rule'\nassert = \"{assertion}\"\n"
    ));
    let automatons = parse(&sources.iter().map(String::as_str).collect::<Vec<_>>());
    let results = verify_joint_invariants(
        &automatons.iter().collect::<Vec<_>>(),
        Budget {
            states: 10,
            time: Duration::from_secs(1),
        },
    );
    assert_eq!(results.len(), 1);
    assert!(matches!(
        results[0].outcome,
        JointOutcome::Incomplete { .. }
    ));
    assert!(results[0].states <= 10);
}

#[test]
fn replay_distinguishes_two_new_values_in_one_request() {
    let pair = r#"
[automaton]
name = "Pair"
states = ["New", "Done"]
initial = "New"
[[state]]
name = "a"
type = "string"
initial = ""
[[state]]
name = "b"
type = "string"
initial = ""
[[action]]
name = "Write"
kind = "input"
from = ["New"]
to = "Done"
params = [{ name = "a", type = "string" }, { name = "b", type = "string" }]
[[invariant]]
name = "equal"
assert = "Parent[Id].status == 'Open' => a == b"
"#;
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Open']\ninitial = 'Open'\n";
    let automatons = parse(&[pair, parent]);
    let refs: Vec<_> = automatons.iter().collect();
    let same = ids(&[("Parent", "p"), ("Pair", "p")]);
    for (b, broken) in [("X", false), ("Y", true)] {
        let replay = replay_joint_invariants(
            &refs,
            "Pair",
            &same,
            &[trace("Pair", "Write", &[("a", "X"), ("b", b)])],
        )
        .unwrap();
        assert_eq!(!replay.broken.is_empty(), broken);
    }
}

#[test]
fn concrete_reference_ids_can_find_an_initially_closed_parent() {
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Closed']\ninitial = 'Closed'\n";
    for id in ["550e8400-e29b-41d4-a716-446655440000", "#2"] {
        let child = format!(
            r#"
[automaton]
name = "Child"
states = ["Ready"]
initial = "Ready"
[[state]]
name = "parent_id"
type = "string"
initial = "{id}"
[[invariant]]
name = "rule"
assert = "Parent[parent_id].status != 'Closed'"
"#
        );
        let result = check(&[parent, &child]);
        let JointOutcome::Broken { path } = &result.outcome else {
            panic!("expected the concrete reference to find Parent: {result}");
        };
        assert_eq!(path.len(), 1, "the rule already fails initially: {result}");
    }
}

#[test]
fn concrete_ids_in_assertions_are_explored_without_string_fields() {
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Closed']\ninitial = 'Closed'\n";
    let child = child_with("Parent[Id].id != 'concrete'");
    let result = check(&[parent, &child]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

#[test]
fn empty_references_never_find_a_record() {
    let parent = "[automaton]\nname = 'Parent'\nstates = ['Closed']\ninitial = 'Closed'\n";
    let child = "[automaton]\nname = 'Child'\nstates = ['Ready']\ninitial = 'Ready'\n[[state]]\nname = 'parent_id'\ntype = 'string'\ninitial = ''\n[[invariant]]\nname = 'rule'\nassert = \"Parent[parent_id].status != 'Closed'\"\n";
    let result = check(&[parent, child]);
    assert!(matches!(result.outcome, JointOutcome::Holds), "{result}");
}
