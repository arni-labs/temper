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
