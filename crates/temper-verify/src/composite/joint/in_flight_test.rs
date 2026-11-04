//! An invariant's walk goes in steps of how much it lets be in flight: a
//! break comes out at the first step that reaches it, and only the last step
//! can say a rule holds.

use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};
use super::*;

fn results(sources: &[&str], room: &Bytes) -> Vec<JointInvariantResult> {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let progress = Progress::default();
    let bound = Bound {
        room,
        progress: &progress,
    };
    verify_joint_invariants(&refs, None, bound)
}

fn one(sources: &[&str]) -> JointInvariantResult {
    let mut found = results(sources, &Bytes::new(1 << 30));
    assert_eq!(found.len(), 1);
    found.remove(0)
}

/// A box that takes `hold`'s guard to be held, under `rule`.
fn boxed(guard: &str, rule: &str) -> String {
    format!(
        r#"
[automaton]
name = "Box"
states = ["Empty", "Held"]
initial = "Empty"

[[action]]
name = "Hold"
kind = "input"
from = ["Empty"]
to = "Held"
guard = "{guard}"

[[invariant]]
name = "rule"
assert = "{rule}"
"#
    )
}

/// A lid that shuts under `guard`.
fn lid(guard: &str) -> String {
    format!(
        r#"
[automaton]
name = "Lid"
states = ["Open", "Shut"]
initial = "Open"

[[action]]
name = "Shut"
kind = "input"
from = ["Open"]
to = "Shut"
{guard}
"#
    )
}

const LID_OPEN: &str = "Lid[Id].status == 'Open'";
const HELD_LID_OPEN: &str = "status == 'Held' => Lid[Id].status == 'Open'";

#[test]
fn a_break_with_one_thing_in_flight_is_found_at_one() {
    // The hold is on its way when the lid shuts.
    let result = one(&[&boxed(LID_OPEN, HELD_LID_OPEN), &lid("")]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    assert_eq!(result.in_flight_bound, Some(1), "{result}");
    assert!(
        result.to_string().contains("at most 1 in flight"),
        "{result}"
    );
}

#[test]
fn a_break_that_needs_two_in_flight_is_found_at_two() {
    // Each guard stops the other's move once it has landed, so only a hold
    // and a shut on their way together break the rule.
    let shut_guard = "guard = \"Box[Id].status == 'Empty'\"";
    let result = one(&[&boxed(LID_OPEN, HELD_LID_OPEN), &lid(shut_guard)]);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    assert_eq!(result.in_flight_bound, Some(2), "{result}");
}

/// A rule nothing breaks, with sends that pile up in flight: each step walks
/// many more states than the one before.
fn holds_with_much_in_flight() -> (String, String) {
    let notes: String = (0..24)
        .map(|note| {
            format!(
                "\n[[action]]\nname = \"Note{note}\"\nkind = \"input\"\nfrom = [\"Empty\"]\n\
                 to = \"Empty\"\nguard = \"{LID_OPEN}\"\n"
            )
        })
        .collect();
    let rule = "status == 'Held' => Lid[Id].status != 'Gone'";
    let held =
        boxed(LID_OPEN, rule).replacen("\n[[invariant]]", &format!("{notes}\n[[invariant]]"), 1);
    (held, lid(""))
}

#[test]
fn a_rule_holds_only_from_the_walk_with_all_in_flight() {
    let (held, lid) = holds_with_much_in_flight();
    let result = one(&[&held, &lid]);
    assert!(result.holds(), "{result}");
    assert_eq!(result.in_flight_bound, Some(3), "{result}");
}

#[test]
fn a_walk_that_runs_out_of_room_never_holds_and_says_how_far_it_got() {
    let (held, lid) = holds_with_much_in_flight();
    let full = one(&[&held, &lid]);
    let mut named = false;
    let mut limit = 1 << 18;
    while limit < 1 << 23 {
        let found = results(&[&held, &lid], &Bytes::new(limit)).remove(0);
        match &found.outcome {
            JointOutcome::Holds => assert_eq!(found.states, full.states, "{found}"),
            JointOutcome::Incomplete { reason } => {
                named |= reason.contains("the walk with at most");
            }
            other => panic!("expected holds or not checked, got {other:?}"),
        }
        limit += limit / 8;
    }
    assert!(named, "some room stops a later walk");
}
