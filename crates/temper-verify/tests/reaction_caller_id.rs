//! A reaction that reaches a move whose parameter is bound to the caller's
//! id (`authenticated_subject`) has the kernel write the id of whoever set
//! the chain off: never empty, never sent by the reaction, and never by a
//! reaction that runs as its own service principal.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::composite::{Bound, Bytes, Progress, verify_joint_invariants};

/// A parent whose Ping makes its child Stamp.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Pinged"]
initial = "Open"

[[action]]
name = "Ping"
kind = "input"
from = ["Open"]
to = "Pinged"

[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// A child only a reaction stamps, with whoever set it off.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Stamped"]
initial = "Waiting"
strict_action_params = true

[[state]]
name = "stamped_by"
type = "string"
initial = ""

[[action]]
name = "Stamp"
kind = "internal"
from = ["Waiting"]
to = "Stamped"
params = [{ name = "stamped_by", type = "string", source = "authenticated_subject" }]

[[invariant]]
name = "stamped_by_no_one"
assert = "status != 'Stamped' || stamped_by == '' || Parent[Id].status == 'Open'"
"#;

const CEDAR: &str = r#"
permit(principal, action == Action::"Ping", resource);
permit(principal, action == Action::"Stamp", resource) when { context has via && context.via != "request" };
"#;

/// Whether every joint invariant of `parent` and `child` holds.
fn holds(parent: &str, child: &str) -> bool {
    let automatons: Vec<Automaton> = [parent, child]
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(CEDAR).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert!(!results.is_empty());
    results.iter().all(|result| result.holds())
}

#[test]
fn a_reaction_writes_the_callers_id_into_a_bound_field() {
    assert!(!holds(PARENT, CHILD));
    assert!(holds(
        PARENT,
        &CHILD.replace("stamped_by == ''", "stamped_by != ''")
    ));
}

#[test]
fn a_reaction_that_sends_a_bound_field_or_runs_as_a_service_is_refused() {
    let named = "target_action = \"Stamp\"\nprincipal = \"stamper\"";
    let sent = "target_action = \"Stamp\"\nargs = { stamped_by = \"'x'\" }";
    for parent in [named, sent] {
        let parent = PARENT.replace("target_action = \"Stamp\"", parent);
        assert!(holds(&parent, CHILD), "{parent}");
    }
}
