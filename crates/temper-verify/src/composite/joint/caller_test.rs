//! A reaction with no declared principal runs as whoever sent its source's
//! action, so Cedar decides it for the caller who passed the source's own
//! Cedar, not for any outside caller at all.

use std::time::Duration;

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// Closing an ask asks its note again, as whoever closed it.
const ASK: &str = r#"
[automaton]
name = "Ask"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "reask"
kind = "entity"
target_entity = "Note"
target_action = "Reask"

[action.triggers.resolve_target]
kind = "same_id"
"#;

const NOTE: &str = r#"
[automaton]
name = "Note"
states = ["Quiet", "Asked"]
initial = "Quiet"

[[action]]
name = "Reask"
kind = "input"
from = ["Quiet"]
to = "Asked"

[[liveness]]
name = "asked"
eventually = "Ask[Id].status == 'Closed' => status == 'Asked'"
"#;

fn budget() -> Budget {
    Budget {
        time: Duration::from_secs(30),
        states: 100_000,
    }
}

/// The note's liveness, with Cedar letting a caller close when `close`
/// holds and ask again when `reask` does.
fn asked(close: &str, reask: &str) -> JointLivenessResult {
    let automatons: Vec<Automaton> = [ASK, NOTE]
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let policies = format!(
        r#"permit(principal, action == Action::"Close", resource) when {{ {close} }};
permit(principal, action == Action::"Reask", resource) when {{ {reask} }};"#
    );
    let analysis = SendAnalysis::new(&policies).expect("fixture policies parse");
    let mut results = verify_joint_liveness(&refs, Some(&analysis), budget());
    assert_eq!(results.len(), 1);
    results.remove(0)
}

const OWNER: &str = r#"principal has role && principal.role == "owner""#;

#[test]
fn a_reaction_is_decided_for_the_caller_who_sent_its_source() {
    // Only an owner may close, and an owner may ask again: whoever closed
    // it, the note is asked again.
    let result = asked(OWNER, OWNER);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_reaction_some_callers_of_its_source_may_not_send_is_not_counted_on() {
    // A guest may close too, and a guest may not ask again.
    let close = r#"principal has role && (principal.role == "owner" || principal.role == "guest")"#;
    let result = asked(close, OWNER);
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}

#[test]
fn a_reaction_is_decided_for_the_type_of_caller_who_sent_its_source() {
    let customer = "principal is Customer";
    let result = asked(customer, customer);
    assert!(result.holds(), "{result}");
}
