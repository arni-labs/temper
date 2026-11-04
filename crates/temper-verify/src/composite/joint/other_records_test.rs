//! What another record of a type can do to the records a walk holds: land on
//! one by naming it, or set off something that comes back into scope.

use temper_spec::automaton::{Automaton, parse_automaton};

use super::missing_tests::{
    ADD, SENDS, TARGET, checked, checked_under, hit_target, holder, not_checked, only_hits_may_send,
};
use super::room::{Bytes, Progress};

use super::*;

const PING: &str = r#"
[automaton]
name = "S"
states = ["Idle", "Pinged"]
initial = "Idle"

[[action]]
name = "Ping"
kind = "input"
from = ["Idle"]
to = "Pinged"
"#;

const TELLS_S: &str = r#"
[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "S"
target_action = "Ping"

[action.triggers.resolve_target]
kind = "same_id"
"#;

#[test]
fn a_reaction_that_leaves_the_scope_for_good_is_a_sure_drop() {
    let result = checked(&[SENDS, &hit_target(TELLS_S), PING]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

#[test]
fn a_reaction_to_another_record_may_come_back_through_a_type_outside_the_scope() {
    let back = r#"
[[action]]
name = "Back"
kind = "internal"
from = ["Sent"]
to = "Start"

[[invariant]]"#;
    let sends = SENDS.replace("[[invariant]]", back);
    let ping = format!(
        r#"{PING}
[[action.triggers]]
name = "back"
kind = "entity"
target_entity = "A"
target_action = "Back"

[action.triggers.resolve_target]
kind = "same_id"
"#
    );
    let result = checked(&[&sends, &hit_target(TELLS_S), &ping]);
    assert!(not_checked(&result), "{result}");
}

/// A B that hits the T it names and tells the T of its own id, and a T that
/// counts the hits, which only a B can send.
const HITS: &str = r#"
[automaton]
name = "B"
states = ["Start", "Went"]
initial = "Start"

[[state]]
name = "f"
type = "string"
initial = ""

[[action]]
name = "Go"
kind = "input"
from = ["Start"]
to = "Went"
params = ["f"]

[[action.triggers]]
name = "mine"
kind = "entity"
target_entity = "T"
target_action = "Mine"
principal = "hits"

[action.triggers.resolve_target]
kind = "same_id"

[[action.triggers]]
name = "hit"
kind = "entity"
target_entity = "T"
target_action = "Hit"
principal = "hits"

[action.triggers.resolve_target]
kind = "field"
id_field = "f"
"#;

const HIT_BY_B: &str = r#"
[automaton]
name = "T"
states = ["Open"]
initial = "Open"

[[state]]
name = "hits"
type = "counter"
initial = 0

[[action]]
name = "Mine"
kind = "input"
from = ["Open"]

[[action]]
name = "Hit"
kind = "input"
from = ["Open"]
effect = ["hits += 1"]

[[invariant]]
name = "rule"
assert = "hits < 2 || B[Id].status == 'Start'"
"#;

#[test]
fn a_second_sender_may_land_on_the_record_the_walk_holds() {
    // One B goes once, so the T it names is hit once. In the runtime a second B
    // can name that T too.
    let policies = only_hits_may_send(r#"Action::"Go""#);
    let result = checked_under(&[HITS, HIT_BY_B], Some(&policies));
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("{result}");
    };
    assert!(reason.contains('B') && reason.contains("T.Hit"), "{reason}");
}

#[test]
fn a_rule_over_records_that_share_an_id_can_still_hold() {
    let shares = r#"
[automaton]
name = "A"
states = ["Start", "Went"]
initial = "Start"

[[action]]
name = "Go"
kind = "input"
from = ["Start"]
to = "Went"

[[action.triggers]]
name = "close"
kind = "entity"
target_entity = "T"
target_action = "Close"

[action.triggers.resolve_target]
kind = "same_id"

[[invariant]]
name = "rule"
assert = "T[Id].status != 'Never'"
"#;
    let result = checked(&[shares, TARGET]);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_second_sender_stops_a_settling_claim_too() {
    let target = HIT_BY_B.replace(
        r#"[[invariant]]
name = "rule"
assert = "hits < 2 || B[Id].status == 'Start'""#,
        r#"[[liveness]]
name = "settles"
eventually = "hits < 2 || B[Id].status == 'Start'""#,
    );
    let automatons: Vec<Automaton> = [HITS, target.as_str()]
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let policies = only_hits_may_send(r#"Action::"Go""#);
    let results = verify_joint_liveness(&refs, Some(&policies), bound);
    assert_eq!(results.len(), 1);
    assert!(
        matches!(results[0].outcome, LivenessOutcome::Incomplete { .. }),
        "{}",
        results[0]
    );
}

#[test]
fn a_goal_that_reads_another_record_does_not_say_it_never_settles() {
    let live = holder(ADD).replace(
        r#"[[invariant]]
name = "rule"
assert = "T[f].status != 'Never'""#,
        r#"[[liveness]]
name = "settles"
eventually = "status != 'Made' || T[f].status == 'Closed'""#,
    );
    let automatons: Vec<Automaton> = [live.as_str(), TARGET]
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_liveness(&refs, None, bound);
    let LivenessOutcome::Incomplete { reason } = &results[0].outcome else {
        panic!("{}", results[0]);
    };
    assert!(reason.contains("A.f"), "{reason}");
}
