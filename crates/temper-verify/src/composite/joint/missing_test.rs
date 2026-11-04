//! A reference that names another id than the record the model holds: the
//! runtime may find that record, in any state, so it reads as unknown unless
//! the walks already cover it.

use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

fn checked(sources: &[&str]) -> JointInvariantResult {
    checked_under(sources, None)
}

/// Lets anyone send what `open` names, and the service `hits` send anything.
fn only_hits_may_send(open: &str) -> SendAnalysis {
    SendAnalysis::new(&format!(
        r#"
permit(principal, action in [{open}], resource);
permit(principal == Agent::"service:hits", action, resource);
"#
    ))
    .unwrap()
}

fn checked_under(sources: &[&str], policies: Option<&SendAnalysis>) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_invariants(&refs, policies, bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn not_checked(result: &JointInvariantResult) -> bool {
    matches!(result.outcome, JointOutcome::Incomplete { .. })
}

/// A record that is opened and closed, and never reaches `Never`.
const TARGET: &str = r#"
[automaton]
name = "T"
states = ["Open", "Closed", "Never"]
initial = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "Rest"
kind = "internal"
from = ["Never"]
to = "Never"
"#;

/// A holder of a reference `f` to a T, with `moves` as its actions and a rule
/// that reads that reference and holds for every T the model reaches, so it
/// says HOLDS when the model reads "no such record" wherever `f` names another.
fn holder(moves: &str) -> String {
    format!(
        r#"
[automaton]
name = "A"
states = ["Start", "Made", "Gone"]
initial = "Start"

[[state]]
name = "f"
type = "string"
initial = ""

{moves}

[[invariant]]
name = "rule"
assert = "T[f].status != 'Never'"
"#
    )
}

const ADD: &str = r#"
[[action]]
name = "Add"
kind = "input"
from = ["Start"]
to = "Made"
params = ["f"]
"#;

#[test]
fn a_reference_set_once_as_its_holder_is_made_breaks_a_rule_it_can_break() {
    // The walks start from the ids being equal, so a break is still found.
    let rule = "T[f].status != 'Never'";
    let holder = holder(ADD).replace(rule, "status != 'Made' || T[f].status != 'Closed'");
    let result = checked(&[&holder, TARGET]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

#[test]
fn a_reference_set_once_as_its_holder_is_made_may_find_another_record() {
    let result = checked(&[&holder(ADD), TARGET]);
    let JointOutcome::Incomplete { reason } = &result.outcome else {
        panic!("{result}");
    };
    assert!(
        reason.contains("A.f") && reason.contains("does not hold"),
        "the reason names what it could not read: {reason}"
    );
}

#[test]
fn two_holders_of_one_record_are_not_one_run_renamed() {
    // One A closes T only as it leaves Made, so with one A no T is closed
    // while its A is in Made. A second A naming the same T closes it then.
    let goes = format!(
        r#"{ADD}
[[action]]
name = "Go"
kind = "input"
from = ["Made"]
to = "Gone"

[[action.triggers]]
name = "close"
kind = "entity"
target_entity = "T"
target_action = "Close"
principal = "hits"

[action.triggers.resolve_target]
kind = "field"
id_field = "f"
"#
    );
    let holder = holder(&goes).replace(
        "T[f].status != 'Never'",
        "status != 'Made' || T[f].status != 'Closed'",
    );
    let policies = only_hits_may_send(r#"Action::"Add", Action::"Go""#);
    let result = checked_under(&[&holder, TARGET], Some(&policies));
    assert!(not_checked(&result), "{result}");
}

#[test]
fn a_reference_set_again_from_the_initial_state_may_find_another_record() {
    let set_again = r#"
[[action]]
name = "Set"
kind = "input"
from = ["Start"]
to = "Start"
params = ["f"]
"#;
    let result = checked(&[&holder(set_again), TARGET]);
    assert!(not_checked(&result), "{result}");
}

#[test]
fn a_reference_set_again_after_going_back_may_find_another_record() {
    let back = format!(
        r#"{ADD}
[[action]]
name = "Reset"
kind = "input"
from = ["Made"]
to = "Start"
"#
    );
    let result = checked(&[&holder(&back), TARGET]);
    assert!(not_checked(&result), "{result}");
}

#[test]
fn a_move_that_stays_put_setting_the_reference_may_find_another_record() {
    let stays = r#"
[[action]]
name = "Point"
kind = "input"
from = ["Start", "Made"]
params = ["f"]
"#;
    let result = checked(&[&holder(stays), TARGET]);
    assert!(not_checked(&result), "{result}");
}

#[test]
fn a_reference_a_later_spawn_stores_may_find_another_record() {
    let spawns = format!(
        r#"{ADD}
[[action]]
name = "Renew"
kind = "input"
from = ["Made"]
to = "Gone"
effect = ["spawn('T', 'Close', f)"]
"#
    );
    let result = checked(&[&holder(&spawns), TARGET]);
    assert!(not_checked(&result), "{result}");
}

#[test]
fn two_references_to_a_type_may_find_two_records() {
    // T1 Closed and T2 Held break this. One held T cannot be both.
    let two = r#"
[automaton]
name = "A"
states = ["Idle", "Made"]
initial = "Idle"

[[state]]
name = "f"
type = "string"
initial = ""

[[state]]
name = "g"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Idle"]
to = "Made"
params = ["f", "g"]

[[invariant]]
name = "rule"
assert = "T[f].status != 'Closed' || T[g].status != 'Held'"
"#;
    let target = r#"
[automaton]
name = "T"
states = ["Open", "Closed", "Held"]
initial = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "Hold"
kind = "input"
from = ["Open"]
to = "Held"
"#;
    let result = checked(&[two, target]);
    assert!(not_checked(&result), "{result}");
}

#[test]
fn a_reference_to_the_holders_own_type_may_find_another_record() {
    // Finish makes the other record Done, and Go is then allowed. The one
    // record the model holds is not Done while it can still Go.
    let own = r#"
[automaton]
name = "A"
states = ["Start", "Made", "Bad", "Done"]
initial = "Start"

[[state]]
name = "other"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Start"]
to = "Made"
params = ["other"]

[[action]]
name = "Finish"
kind = "input"
from = ["Made"]
to = "Done"

[[action]]
name = "Go"
kind = "input"
from = ["Made"]
to = "Bad"
guard = "A[other].status == 'Done'"

[[invariant]]
name = "rule"
assert = "status != 'Bad' || T[Id].status == 'Never'"
"#;
    let result = checked(&[own, TARGET]);
    assert!(not_checked(&result), "{result}");
}

#[test]
fn a_named_type_that_acts_back_may_land_on_another_holder() {
    let back = r#"
[automaton]
name = "T"
states = ["Open", "Closed", "Never"]
initial = "Open"

[[state]]
name = "by"
type = "string"
initial = ""

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "A"
target_action = "Told"

[action.triggers.resolve_target]
kind = "field"
id_field = "by"

[[action]]
name = "Rest"
kind = "internal"
from = ["Never"]
to = "Never"
"#;
    let told = format!(
        r#"{ADD}
[[action]]
name = "Told"
kind = "input"
from = ["Made"]
to = "Gone"
"#
    );
    let result = checked(&[&holder(&told), back]);
    assert!(not_checked(&result), "{result}");
}

/// A post that opens the thread it names when that is not made yet.
const POST: &str = r#"
[automaton]
name = "Post"
states = ["Unmade", "Posted"]
initial = "Unmade"

[[state]]
name = "thread"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Posted"
params = ["thread"]

[[action.triggers]]
name = "open_thread"
kind = "entity"
target_entity = "Thread"
target_action = "Add"
guard = "Thread[thread].status == 'Unmade'"

[action.triggers.args]
title = "thread"

[action.triggers.resolve_target]
kind = "field"
id_field = "thread"

[[invariant]]
name = "ThreadExists"
assert = "status == 'Unmade' || thread == '' || Thread[thread].status != 'Unmade'"
"#;

const THREAD: &str = r#"
[automaton]
name = "Thread"
states = ["Unmade", "Open"]
initial = "Unmade"

[[state]]
name = "title"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Open"
params = ["title"]
"#;

#[test]
fn a_post_that_opens_the_thread_it_names_may_find_another_thread() {
    let result = checked(&[POST, THREAD]);
    assert!(not_checked(&result), "{result}");
}

/// Sends a reaction to the T that `f` names, and reads whether it arrived.
const SENDS: &str = r#"
[automaton]
name = "A"
states = ["Start", "Sent"]
initial = "Start"

[[state]]
name = "f"
type = "string"
initial = ""

[[action]]
name = "Send"
kind = "input"
from = ["Start"]
to = "Sent"
guard = "f != ''"
params = ["f"]

[[action.triggers]]
name = "hit"
kind = "entity"
target_entity = "T"
target_action = "Hit"

[action.triggers.resolve_target]
kind = "field"
id_field = "f"

[[invariant]]
name = "rule"
assert = "status != 'Sent' || T[f].status == 'Hit'"
"#;

/// `triggers` is added to Hit.
fn hit_target(triggers: &str) -> String {
    format!(
        r#"
[automaton]
name = "T"
states = ["Open", "Hit"]
initial = "Open"

[[action]]
name = "Hit"
kind = "input"
from = ["Open"]
to = "Hit"
{triggers}
"#
    )
}

#[test]
fn a_reaction_to_another_record_that_keeps_to_itself_is_a_sure_drop() {
    let result = checked(&[SENDS, &hit_target("")]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}

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

/// A B that opens the T of its own id and hits the T it names, and that T,
/// which only a B can hit.
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
states = ["Open", "Once"]
initial = "Open"

[[action]]
name = "Mine"
kind = "input"
from = ["Open", "Once"]

[[action]]
name = "Hit"
kind = "input"
from = ["Open"]
to = "Once"

[[invariant]]
name = "rule"
assert = "status != 'Once' || B[Id].status != 'Start'"
"#;

#[test]
fn a_second_sender_may_land_on_the_record_the_walk_holds() {
    // With one B, its T is hit only once it has gone. In the runtime another B
    // can name that T while this one is in Start.
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
assert = "status != 'Once' || B[Id].status != 'Start'""#,
        r#"[[liveness]]
name = "settles"
eventually = "status != 'Once' || B[Id].status != 'Start'""#,
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
