//! A rule read one pair of records at a time: the walk holds one record of
//! each type it reads, and what other records do reaches them from outside.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

/// Anyone may send every move but `Down`, which only a reaction may.
const POLICIES: &str = r#"
permit(principal, action in [Action::"Add", Action::"Start", Action::"Kick", Action::"Make"], resource);
permit(principal, action == Action::"Down", resource)
when { context has via && context.via != "request" };
"#;

/// Anyone may send every move.
const OPEN: &str = "permit(principal, action, resource);";

/// A record that names a `T` and, depending on `guard`, goes `On` or kicks it.
fn owner(rule: &str, guard: &str, kicks: bool) -> Automaton {
    let kick = if kicks {
        r#"
[[action]]
name = "Kick"
kind = "input"
from = ["Waiting"]
to = "Kicked"

[[action.triggers]]
name = "kick"
kind = "entity"
guard = "t != ''"
target_entity = "T"
target_action = "Down"
[action.triggers.resolve_target]
kind = "field"
id_field = "t"
"#
    } else {
        ""
    };
    parse_automaton(&format!(
        r#"
[automaton]
name = "A"
states = ["Unmade", "Waiting", "On", "Kicked"]
initial = "Unmade"

[[state]]
name = "t"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Waiting"
params = ["t"]

[[action]]
name = "Start"
kind = "input"
from = ["Waiting"]
to = "On"
guard = "{guard}"
{kick}
[[invariant]]
name = "Rule"
assert = "{rule}"
"#
    ))
    .expect("fixture parses")
}

/// The record `A` names. `Make` brings it up, and `Down` takes it down.
fn target() -> Automaton {
    parse_automaton(
        r#"
[automaton]
name = "T"
states = ["Unmade", "Up", "Down"]
initial = "Unmade"

[[action]]
name = "Make"
kind = "input"
from = ["Unmade"]
to = "Up"

[[action]]
name = "Down"
kind = "input"
from = ["Up"]
to = "Down"
"#,
    )
    .expect("fixture parses")
}

fn check(automatons: &[Automaton], policies: &str) -> JointInvariantResult {
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1, "{results:?}");
    results.remove(0)
}

const STAYS_UP: &str = "status == 'On' && !empty(t) => T[t].status == 'Up'";
const NOT_DOWN: &str = "status == 'On' && !empty(t) => T[t].status != 'Down'";

/// One `A` can't both go `On` and kick. Two can, naming one `T`: the first
/// goes `On`, the second kicks, and the rule is false.
#[test]
fn a_rule_false_only_with_a_second_record_never_holds() {
    let result = check(
        &[owner(NOT_DOWN, "T[t].status == 'Up'", true), target()],
        POLICIES,
    );
    assert!(
        !matches!(result.outcome, JointOutcome::Holds),
        "a second A breaks it: {result}"
    );
}

/// `Up` never ends here, and an `A` goes `On` only with its own `T` up. It
/// holds however many records there are, and the field can name any of them.
#[test]
fn a_rule_true_of_every_pair_holds_though_its_field_can_name_another_record() {
    let mut up_for_good = target();
    up_for_good.actions.retain(|action| action.name != "Down");
    up_for_good.automaton.states.retain(|state| state != "Down");
    let result = check(
        &[owner(STAYS_UP, "T[t].status == 'Up'", false), up_for_good],
        OPEN,
    );
    assert!(matches!(result.outcome, JointOutcome::Holds), "{result}");
}

/// The pair's own sure moves break it: the `T` it names goes down.
#[test]
fn a_break_on_the_pairs_own_moves_stays_broken() {
    let result = check(
        &[owner(STAYS_UP, "T[t].status == 'Up'", false), target()],
        OPEN,
    );
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// A field can name a record nobody made: every id is free for the taking.
#[test]
fn a_field_naming_a_record_nobody_made_is_broken() {
    let rule = "status != 'Unmade' && !empty(t) => T[t].status in ['Up', 'Down']";
    let result = check(&[owner(rule, "t != ''", false), target()], OPEN);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

/// The walk can't hold both `T`s: with one, `x` and `y` name it or they don't.
#[test]
fn a_rule_reading_two_records_of_one_type_is_not_checked() {
    let rule = "T[x].status == 'Up' => T[y].status == 'Up'";
    let reads_two = parse_automaton(&format!(
        r#"
[automaton]
name = "A"
states = ["Waiting"]
initial = "Waiting"

[[state]]
name = "x"
type = "string"
initial = ""

[[state]]
name = "y"
type = "string"
initial = ""

[[action]]
name = "Name"
kind = "input"
from = ["Waiting"]
to = "Waiting"
params = ["x", "y"]

[[invariant]]
name = "Rule"
assert = "{rule}"
"#
    ))
    .expect("fixture parses");
    let mut up_for_good = target();
    up_for_good.actions.retain(|action| action.name != "Down");
    up_for_good.automaton.states.retain(|state| state != "Down");
    let result = check(&[reads_two, up_for_good], OPEN);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("x can name a T that is up and y one that isn't: {result}");
    };
    assert!(reason.contains("two records of T"), "{reason}");
}

/// The walk holds one `A`, so it can't read another through its own field.
#[test]
fn a_rule_reading_another_record_of_its_own_type_is_not_checked() {
    let reads_own = parse_automaton(
        r#"
[automaton]
name = "A"
states = ["Waiting", "On"]
initial = "Waiting"

[[state]]
name = "peer"
type = "string"
initial = ""

[[action]]
name = "Name"
kind = "input"
from = ["Waiting"]
to = "On"
params = ["peer"]

[[invariant]]
name = "Rule"
assert = "status == 'On' && !empty(peer) => A[peer].status == 'On'"
"#,
    )
    .expect("fixture parses");
    let result = check(&[reads_own], OPEN);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("peer can name an A still waiting: {result}");
    };
    assert!(reason.contains("two records of A"), "{reason}");
}
