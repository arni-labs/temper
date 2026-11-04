//! A reference that names another id than the record the model holds: the
//! runtime may find that record, in any state, so it reads as unknown unless
//! the walks already cover it.

use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

use super::*;

fn checked(sources: &[&str]) -> JointInvariantResult {
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
    let mut results = verify_joint_invariants(&refs, None, bound);
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
fn a_reference_set_once_as_its_holder_is_made_reads_no_such_record() {
    assert!(checked(&[&holder(ADD), TARGET]).holds());
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

/// A post that opens the thread it names when that is not made yet, copying
/// the thread's id into a field of the thread.
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
fn a_reference_is_one_name_though_its_id_is_copied_into_a_field() {
    // Literal classes put the thread's id and its title together. That is one
    // value in two places, not a second name for a thread.
    let result = checked(&[POST, THREAD]);
    assert!(result.holds(), "{result}");
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

#[test]
fn a_reaction_to_another_record_may_come_back_into_scope() {
    let reacts = r#"
[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "S"
target_action = "Ping"

[action.triggers.resolve_target]
kind = "same_id"
"#;
    let other = r#"
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
    let result = checked(&[SENDS, &hit_target(reacts), other]);
    assert!(not_checked(&result), "{result}");
}
