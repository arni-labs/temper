//! A move the kernel makes itself (a reaction, a timer, a module's answer)
//! that reaches an action whose parameter the kernel binds (`source`) has the
//! kernel bind it from whoever set the chain off. No one may send it, and a
//! reaction that runs as its own service principal names no caller, so the
//! kernel refuses the move.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::composite::{
    Bound, Bytes, LivenessOutcome, Progress, verify_joint_invariants, verify_joint_liveness,
};

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

/// A record whose Ping makes it Stamp, as the reaction's own principal.
const AS_SERVICE: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Pinged", "Stamped"]
initial = "Waiting"
strict_action_params = true

[[state]]
name = "stamped_by"
type = "string"
initial = ""

[[action]]
name = "Ping"
kind = "input"
from = ["Waiting"]
to = "Pinged"

[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"
principal = "stamper"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Stamp"
kind = "internal"
from = ["Pinged"]
to = "Stamped"
params = [{ name = "stamped_by", type = "string", source = "authenticated_subject" }]

[[liveness]]
name = "gets_stamped"
eventually = "status != 'Pinged'"
"#;

const CEDAR: &str = r#"
permit(principal, action == Action::"Ping", resource);
permit(principal, action == Action::"Stamp", resource) when { context has via && context.via != "request" };
"#;

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

/// Whether every joint invariant of `parent` and `child` holds.
fn holds(parent: &str, child: &str) -> bool {
    let automatons = parsed(&[parent, child]);
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
fn a_reaction_writes_the_callers_id_or_label_into_a_bound_field() {
    let nonempty = CHILD.replace("stamped_by == ''", "stamped_by != ''");
    assert!(!holds(PARENT, CHILD));
    assert!(holds(PARENT, &nonempty));
    let label = CHILD.replace("authenticated_subject", "authenticated_agent_label");
    assert!(!holds(PARENT, &label));
}

#[test]
fn a_reaction_that_sends_a_bound_field_or_runs_as_a_service_is_refused() {
    let named = PARENT.replace(
        "target_action = \"Stamp\"",
        "target_action = \"Stamp\"\nprincipal = \"stamper\"",
    );
    let nonempty = CHILD.replace("stamped_by == ''", "stamped_by != ''");
    assert!(holds(&named, &nonempty));
    let sent = PARENT.replace(
        "target_action = \"Stamp\"",
        "target_action = \"Stamp\"\nargs = { stamped_by = \"'x'\" }",
    );
    assert!(holds(&sent, CHILD));
}

/// The liveness outcomes of `source` under `policies`.
fn liveness(source: &str, policies: &str) -> Vec<LivenessOutcome> {
    let automatons = parsed(&[source]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_liveness(&refs, Some(&analysis), bound);
    assert!(!results.is_empty());
    results.into_iter().map(|result| result.outcome).collect()
}

/// Nothing reads the bound field here, and the kernel still refuses.
#[test]
fn a_move_the_kernel_always_refuses_never_settles() {
    let sent = AS_SERVICE.replace("principal = \"stamper\"", "args = { stamped_by = \"'x'\" }");
    for source in [AS_SERVICE, &sent] {
        let outcomes = liveness(source, "permit(principal, action, resource);");
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, LivenessOutcome::Stuck { .. })),
            "{source}: {outcomes:#?}"
        );
    }
}

/// A sender that surely names a caller (an agent: only a public endpoint's
/// anonymous caller names none) has the kernel surely bind, so the move is
/// surely taken and the rule surely holds.
#[test]
fn a_reaction_for_a_sender_who_names_a_caller_surely_binds() {
    let inherits = AS_SERVICE.replace("principal = \"stamper\"\n", "");
    assert_ne!(inherits, AS_SERVICE);
    let outcomes = liveness(&inherits, "permit(principal is Agent, action, resource);");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, LivenessOutcome::Holds)),
        "{outcomes:#?}"
    );
}
