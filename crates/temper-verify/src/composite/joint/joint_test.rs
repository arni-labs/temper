use std::collections::BTreeMap;
use std::time::Duration;

use temper_authz::SendAnalysis;
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
    check_under(sources, None)
}

/// The one result for `sources`, with `policies` (Cedar) deciding each move
/// as its sender.
fn check_under(sources: &[&str], policies: Option<&str>) -> JointInvariantResult {
    let automatons = parse(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = policies.map(|text| SendAnalysis::new(text).expect("fixture policies parse"));
    let budget = Budget {
        time: Duration::from_secs(30),
        states: 100_000,
    };
    let mut results = verify_joint_invariants(&refs, analysis.as_ref(), budget);
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

/// A parent set once, whose `tell` runs as its own service, and Cedar that
/// lets only that service send the child's Copy.
fn parent_told_once() -> String {
    parent(false)
        .replace(
            "from = [\"Open\"]\nto = \"Open\"\nparams",
            "from = [\"Open\"]\nto = \"Closed\"\nparams",
        )
        .replace(
            "name = \"tell\"\nkind = \"entity\"",
            "name = \"tell\"\nkind = \"entity\"\nprincipal = \"tell\"",
        )
}

const ONLY_TELL_COPIES: &str = r#"
permit(principal, action in [Action::"Set", Action::"Close", Action::"Finish"], resource);
permit(principal == Agent::"service:tell", action == Action::"Copy", resource is Child);
"#;

#[test]
fn a_copy_only_the_parents_service_may_send_holds_under_its_cedar() {
    let child = child_with("status == 'Done' => Parent[Id].value == value");
    let parent = parent_told_once();
    assert!(
        !check(&[&parent, &child]).holds(),
        "without Cedar anyone may send Copy"
    );
    let result = check_under(&[&parent, &child], Some(ONLY_TELL_COPIES));
    assert!(result.holds(), "{result}");
}

#[test]
fn an_action_a_liveness_relies_on_is_sent_whatever_cedar_says() {
    let child = format!(
        "{}\n[[liveness]]\nname = \"moves_on\"\neventually = \"status != 'Waiting'\"\nrelies_on = [\"Copy\"]\n",
        child_with("status == 'Done' => Parent[Id].value == value")
    );
    let parent = parent_told_once();
    let result = check_under(&[&parent, &child], Some(ONLY_TELL_COPIES));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert!(
        path.iter()
            .filter_map(|step| step.action.as_deref())
            .any(|a| a.starts_with("Child.Copy")),
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
    assert!(verify_joint_invariants(&refs, None, budget).is_empty());
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
fn a_value_only_the_invariant_names_is_one_a_request_may_send() {
    let child = child_with("Parent[Id].value == 'secret' => status == 'Waiting'");
    let parent = parent(false);
    let result = check(&[&parent, &child]);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("expected a broken invariant, got {result}");
    };
    assert!(
        path.iter()
            .filter_map(|s| s.action.as_deref())
            .any(|m| m.starts_with("Parent.Set") && m.contains("'secret'")),
        "{result}"
    );
}

#[test]
fn a_sequence_copied_by_effect_keeps_its_actual_value() {
    let parent = r#"
[automaton]
name = "Parent"
states = ["Zero", "One", "Two", "Copied"]
initial = "Zero"
[[state]]
name = "sequence"
type = "counter"
initial = 0
[[state]]
name = "snapshot"
type = "counter"
initial = 0
[[action]]
name = "First"
kind = "input"
from = ["Zero"]
to = "One"
effect = ["sequence += 1"]
[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Child"
target_action = "Copy"
principal = "counter"
[action.triggers.args]
sequence = "sequence"
[action.triggers.resolve_target]
kind = "same_id"
[[action]]
name = "Second"
kind = "input"
from = ["One"]
to = "Two"
effect = ["sequence += 1"]
[[action]]
name = "Snapshot"
kind = "input"
from = ["Two"]
to = "Copied"
effect = ["snapshot = sequence"]
"#;
    let child = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done"]
initial = "Waiting"
[[state]]
name = "sequence"
type = "counter"
initial = 0
[[action]]
name = "Copy"
kind = "input"
from = ["Waiting", "Done"]
to = "Done"
params = [{ name = "sequence", type = "counter" }]
effect = ["sequence = params.sequence"]
[[action.constraints]]
kind = "param_greater_than_field"
param = "sequence"
field = "sequence"
[[invariant]]
name = "snapshot_bound"
assert = "status == 'Done' => Parent[Id].snapshot <= 1"
"#;
    let policies = r#"
permit(principal, action in [Action::"First", Action::"Second", Action::"Snapshot"], resource);
permit(principal, action == Action::"Copy", resource is Child) when {
    principal == Agent::"service:counter" && context has triggerSourceEntityId &&
    resource has id && context.triggerSourceEntityId == resource.id
};
"#;
    let result = check_under(&[parent, child], Some(policies));
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("the copied sequence must violate the invariant: {result}");
    };
    assert!(
        path.iter().any(|step| step
            .action
            .as_deref()
            .is_some_and(|a| a.starts_with("Parent.Snapshot"))),
        "{result}"
    );
    assert!(
        path.last().unwrap().state.contains("snapshot=2"),
        "{result}"
    );
}
