//! One-type invariants and premises blocked by Cedar.

use super::*;

/// A counter whose rule reads only its own count, which goes up by one from
/// zero.
fn counter(rule: &str) -> String {
    format!(
        r#"
[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Increment"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["count += 1"]

[[invariant]]
name = "Small"
assert = "{rule}"
"#
    )
}

#[test]
fn an_invariant_on_one_type_is_proved_or_fails_after_its_move() {
    let verdict = invariant(&[&counter("count >= 0")], TIME);
    assert!(matches!(verdict, JointVerdict::Proved(..)), "{verdict}");
    assert!(verdict.holds(), "{verdict}");
    assert_eq!(verdict.rule(), "Counter.Small");

    let verdict = invariant(&[&counter("count < 2")], TIME);
    let JointVerdict::Invariant(walked, Some(tried)) = &verdict else {
        panic!("walked with the failed proof: {verdict}");
    };
    assert!(!tried.proved());
    assert!(!walked.holds(), "{walked}");
    let text = verdict.to_string();
    assert!(text.contains("Counter.Increment"), "{text}");
}

/// A gate that shuts and locks, whose rule `rule` reads its status: no
/// move leads to Sealed.
fn gate(rule: &str) -> String {
    format!(
        r#"
[automaton]
name = "Gate"
states = ["Open", "Shut", "Sealed"]
initial = "Open"

[[state]]
name = "locked"
type = "bool"
initial = false

[[action]]
name = "Shut"
kind = "input"
from = ["Open"]
to = "Shut"
effect = ["locked = true"]

[[invariant]]
name = "Rule"
assert = "{rule}"
"#
    )
}

/// The one invariant's verdict with `cedar` deciding each move.
fn invariant_under(source: &str, cedar: &str) -> JointVerdict {
    let automaton = parse_automaton(source).expect("fixture parses");
    let policies = temper_authz::SendAnalysis::new(cedar).expect("Cedar parses");
    let bound = crate::composite::joint::room::test_bound();
    let mut invariants = check_joint(&[&automaton], Some(&policies), bound, TIME).invariants;
    assert_eq!(invariants.len(), 1, "{invariants:?}");
    invariants.remove(0)
}

const SHUT_IS_LOCKED: &str = "status in ['Shut'] => locked == true";
const LETS_SHUT: &str = r#"permit(principal, action == Action::"Shut", resource is Gate);"#;

#[test]
fn a_premise_only_cedar_keeps_from_a_run_waits_on_cedar() {
    // Cedar lets someone shut the gate: a run meets the premise.
    let verdict = invariant_under(&gate(SHUT_IS_LOCKED), LETS_SHUT);
    assert!(
        matches!(verdict, JointVerdict::Proved(_, Premise::Met { .. })),
        "{verdict}"
    );
    // Cedar lets nobody shut it: no run meets the premise as Cedar stands,
    // but with every sender allowed one does and the rule holds.
    let verdict = invariant_under(&gate(SHUT_IS_LOCKED), "");
    let JointVerdict::WaitsOnCedar(open, unsent) = &verdict else {
        panic!("waits on Cedar: {verdict}");
    };
    assert!(open.holds(), "{open}");
    assert_eq!(unsent, &["Gate.Shut".to_string()]);
    assert!(verdict.holds(), "{verdict}");
    let text = verdict.to_string();
    assert!(text.contains("Cedar lets nobody send Gate.Shut"), "{text}");
}

#[test]
fn a_rule_that_breaks_with_every_sender_allowed_proves_nothing() {
    // Shutting locks the gate, so the rule holds only while nobody may.
    let verdict = invariant_under(&gate("status in ['Shut'] => locked == false"), "");
    assert_eq!(verdict.tag(), "VACUOUS", "{verdict}");
    assert!(!verdict.holds(), "{verdict}");
    let text = verdict.to_string();
    assert!(!text.contains("every sender allowed"), "{text}");
}

#[test]
fn a_premise_dead_in_the_spec_proves_nothing() {
    let verdict = invariant_under(&gate("status in ['Sealed'] => locked == true"), "");
    assert_eq!(verdict.tag(), "VACUOUS", "{verdict}");
    assert!(!verdict.holds(), "{verdict}");
    let text = verdict.to_string();
    assert!(!text.contains("every sender allowed"), "{text}");
}

/// Four counters stepped in Start by a move each, and a chain of statuses
/// down to End, which Finish reaches with `done` set. Go comes first, so a
/// search that takes the latest state found first goes down the counters.
const DEEP: &str = r#"
[automaton]
name = "Deep"
states = ["Start", "One", "Two", "Three", "End"]
initial = "Start"

[[state]]
name = "done"
type = "bool"
initial = false

[[state]]
name = "a"
type = "counter"
initial = 0

[[state]]
name = "b"
type = "counter"
initial = 0

[[state]]
name = "c"
type = "counter"
initial = 0

[[state]]
name = "d"
type = "counter"
initial = 0

[[action]]
name = "Go"
kind = "input"
from = ["Start"]
to = "One"

[[action]]
name = "A"
kind = "input"
from = ["Start"]
effect = ["a += 1"]

[[action]]
name = "B"
kind = "input"
from = ["Start"]
effect = ["b += 1"]

[[action]]
name = "C"
kind = "input"
from = ["Start"]
effect = ["c += 1"]

[[action]]
name = "D"
kind = "input"
from = ["Start"]
effect = ["d += 1"]

[[action]]
name = "OnToTwo"
kind = "input"
from = ["One"]
to = "Two"

[[action]]
name = "OnToThree"
kind = "input"
from = ["Two"]
to = "Three"

[[action]]
name = "Finish"
kind = "input"
from = ["Three"]
to = "End"
effect = ["done = true"]

[[invariant]]
name = "EndIsDone"
assert = "status == 'End' => done == true && a >= 0 && b >= 0 && c >= 0 && d >= 0"
"#;

#[test]
fn the_walk_for_a_premise_heads_for_the_statuses_where_it_may_hold() {
    let verdict = invariant(&[DEEP], TIME);
    let JointVerdict::Proved(_, Premise::Met { states, .. }) = &verdict else {
        panic!("proved, and a run meets its premise: {verdict}");
    };
    // The rule reads Start's counters, so the walk keeps them, and they
    // alone make over a hundred states; the walk takes Start, then each
    // status down the chain.
    assert!(*states <= 12, "{verdict}");
}
