use temper_spec::automaton::parse_automaton;

use super::super::{Hints, prove};

/// A meter whose limit is raised while it is open, and which a timer in
/// Waiting or in Late expires, passing its limit as the amount it used.
/// Expire is taken only by those timers; `effect` is its effect.
fn meter(effect: &str) -> String {
    format!(
        r#"
[automaton]
name = "Meter"
states = ["Open", "Waiting", "Late", "Shut"]
initial = "Open"

[[state]]
name = "used"
type = "counter"
initial = 0

[[state]]
name = "limit"
type = "counter"
initial = 0

[[action]]
name = "Raise"
kind = "input"
from = ["Open"]
effect = ["limit += 1"]

[[action]]
name = "Wait"
kind = "input"
from = ["Open"]
to = "Waiting"

[[action]]
name = "Lag"
kind = "input"
from = ["Waiting"]
to = "Late"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting", "Late"]
to = "Shut"
params = [{{ name = "amount", type = "counter" }}]
{effect}

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "Expire"

[state_timeout.args]
amount = "limit"

[[state_timeout]]
state = "Late"
after_seconds = 60
on_timeout = "Expire"

[state_timeout.args]
amount = "limit"

[[invariant]]
name = "UsedWithinLimit"
assert = "used <= limit"
"#
    )
}

/// The names of the obligations the proof of `spec`'s rule checked, which
/// must hold.
fn proved(spec: &str) -> Vec<String> {
    let automaton = parse_automaton(spec).expect("fixture parses");
    let proofs = prove(&[&automaton], None, &Hints::default());
    assert_eq!(proofs.len(), 1);
    assert!(proofs[0].proved(), "{:?}", proofs[0].shortfall());
    proofs[0]
        .obligations
        .iter()
        .map(|o| o.name.clone())
        .collect()
}

#[test]
fn a_general_move_that_keeps_the_rule_stands_for_every_way_it_is_taken() {
    let checked = proved(&meter(""));
    assert!(
        checked.contains(&"Meter.Expire (every way it is taken)".to_string()),
        "{checked:?}"
    );
    assert!(!checked.iter().any(|n| n.contains("timer")), "{checked:?}");
}

#[test]
fn a_general_move_that_breaks_the_rule_is_checked_way_by_way() {
    // Expire with any amount breaks the rule; each timer passes the limit.
    let checked = proved(&meter(r#"effect = ["used = params.amount"]"#));
    for way in [
        "Meter.Expire (timer in Waiting)",
        "Meter.Expire (timer in Late)",
    ] {
        assert!(checked.contains(&way.to_string()), "{checked:?}");
    }
    assert!(
        !checked.iter().any(|n| n.contains("every way")),
        "{checked:?}"
    );
}

/// A job that, as it starts, calls two modules, either of which may answer
/// that it finished. A call that fails fires nothing.
const CALLS: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Running", "Done"]
initial = "Idle"

[[state]]
name = "started"
type = "bool"
initial = false

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
effect = ["started = true"]

[[action.triggers]]
name = "first"
kind = "wasm"
module = "one"
on_success = "Finish"

[[action.triggers]]
name = "second"
kind = "wasm"
module = "two"
on_success = "Finish"

[[action]]
name = "Finish"
kind = "internal"
from = ["Running"]
to = "Done"

[[invariant]]
name = "DoneWasStarted"
assert = "status == 'Done' => started == true"
"#;

#[test]
fn each_move_that_takes_no_action_is_checked() {
    let checked = proved(CALLS);
    for way in ["Job@one fails", "Job@two fails"] {
        assert!(checked.contains(&way.to_string()), "{checked:?}");
    }
}

/// A meter its timers shut, which may be archived once shut. Proving that
/// an archived meter was shut needs the guess that a shut one is.
const SHUT: &str = r#"
[automaton]
name = "Meter"
states = ["Open", "Waiting", "Late", "Shut", "Archived"]
initial = "Open"

[[state]]
name = "shut"
type = "bool"
initial = false

[[action]]
name = "Wait"
kind = "input"
from = ["Open"]
to = "Waiting"

[[action]]
name = "Lag"
kind = "input"
from = ["Waiting"]
to = "Late"

[[action]]
name = "Expire"
kind = "internal"
from = ["Waiting", "Late"]
to = "Shut"
effect = ["shut = true"]

[[state_timeout]]
state = "Waiting"
after_seconds = 60
on_timeout = "Expire"

[[state_timeout]]
state = "Late"
after_seconds = 60
on_timeout = "Expire"

[[action]]
name = "Archive"
kind = "input"
from = ["Shut"]
to = "Archived"

[[invariant]]
name = "ArchivedWasShut"
assert = "status == 'Archived' => shut == true"
"#;

#[test]
fn a_general_move_stands_again_once_the_guesses_it_broke_are_dropped() {
    // Expire breaks the guess that a shut meter is not shut, every way it
    // is taken; once that guess is dropped, it keeps the rest.
    let checked = proved(SHUT);
    assert!(
        checked.contains(&"Meter.Expire (every way it is taken)".to_string()),
        "{checked:?}"
    );
    assert!(!checked.iter().any(|n| n.contains("timer")), "{checked:?}");
}
