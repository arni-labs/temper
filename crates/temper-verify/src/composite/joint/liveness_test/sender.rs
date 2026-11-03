use super::*;

/// A note, addressed when it is for someone: its Add's own reaction makes
/// Address, as whoever added it.
const NOTE: &str = r#"
[automaton]
name = "Note"
states = ["Unmade", "Posted", "Waiting"]
initial = "Unmade"

[[state]]
name = "to"
type = "string"
initial = ""

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Posted"
params = [{ name = "to", type = "string" }]

[[action.triggers]]
name = "address"
kind = "entity"
target_entity = "Note"
target_action = "Address"
guard = "to != ''"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Address"
kind = "input"
from = ["Posted"]
to = "Waiting"

[[liveness]]
name = "AddressedNoteWaits"
eventually = "to == '' || status != 'Posted'"
"#;

/// Members add notes; Address follows only from a reaction; nothing is
/// allowed to a visitor.
const MEMBERS_ADD: &str = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && principal.role == "member" };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action, resource is Note)
when { principal has role && principal.role == "visitor" };
"#;

fn check(policies: &str) -> JointLivenessResult {
    let note = parse_automaton(NOTE).expect("fixture parses");
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&note], Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_chain_runs_as_the_one_principal_who_sent_its_request() {
    // Only a member may Add, and Cedar refuses Address only to a visitor,
    // who may not Add: no sender's Add is followed by a refused Address.
    let result = check(MEMBERS_ADD);
    assert!(result.holds(), "{result}");
}

/// Members and visitors add notes, and Cedar refuses Address to a visitor.
const VISITORS_ADD: &str = r#"
permit(principal, action == Action::"Add", resource is Note)
when { principal has role && (principal.role == "member" || principal.role == "visitor") };
permit(principal, action == Action::"Address", resource is Note)
when { context has via && context.via != "request" };
forbid(principal, action == Action::"Address", resource is Note)
when { principal has role && principal.role == "visitor" };
"#;

#[test]
fn a_chain_cedar_refuses_its_own_sender_halfway_never_settles() {
    // A visitor's Add goes through and its Address is refused, so its note
    // stays Posted.
    let result = check(VISITORS_ADD);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Note=Posted")),
        "{result}"
    );
}
