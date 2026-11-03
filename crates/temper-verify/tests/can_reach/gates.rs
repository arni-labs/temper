//! A move gated on another record is explored as one that may fire, but the
//! gate may never open. A route that needs such a move cannot be proved
//! from this record alone; a gate off the route changes nothing.

use super::model_check;

/// A draft is published once its file is ready.
const GATED_ROUTE: &str = r#"
[automaton]
name = "Gated"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "File[file_id].status in ['Ready']"

[[liveness]]
name = "can_publish"
from = ["Draft"]
can_reach = ["Published"]
"#;

/// A draft is published freely; linking it afterwards waits on its file.
const GATE_OFF_THE_ROUTE: &str = r#"
[automaton]
name = "Gated"
states = ["Draft", "Published", "Linked"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"

[[action]]
name = "Link"
from = ["Published"]
to = "Linked"
guard = "File[file_id].status in ['Ready']"

[[liveness]]
name = "can_publish"
from = ["Draft"]
can_reach = ["Published"]
"#;

/// A draft is published once its file is ready, or by an editor's override.
const GATE_BESIDE_AN_OPEN_ROUTE: &str = r#"
[automaton]
name = "Gated"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
guard = "File[file_id].status in ['Ready']"

[[action]]
name = "Override"
from = ["Draft"]
to = "Published"

[[liveness]]
name = "can_publish"
from = ["Draft"]
can_reach = ["Published"]
"#;

#[test]
fn a_route_only_through_a_gate_another_record_holds_is_not_proved() {
    let l1 = model_check(GATED_ROUTE);
    assert!(!l1.passed, "{}", l1.summary);
    assert!(
        l1.summary.contains("rule 'can_publish' not proved"),
        "{}",
        l1.summary
    );
    assert!(
        l1.summary.contains("Publish"),
        "names the gate: {}",
        l1.summary
    );
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(
        result.counterexamples.is_empty(),
        "no record is known to be cut off"
    );
}

#[test]
fn a_gate_off_the_route_does_not_stop_a_pass() {
    let l1 = model_check(GATE_OFF_THE_ROUTE);
    assert!(l1.passed, "{}", l1.summary);
}

#[test]
fn a_gate_beside_an_open_route_does_not_stop_a_pass() {
    let l1 = model_check(GATE_BESIDE_AN_OPEN_ROUTE);
    assert!(l1.passed, "{}", l1.summary);
}
