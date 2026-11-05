use super::*;

/// A ping starts its job by a reaction; the job's Start finishes it by
/// another. Only Job is in the rule's scope, so Start arrives from outside.
const PING: &str = r#"
[automaton]
name = "Ping"
states = ["Unmade", "Sent"]
initial = "Unmade"

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Sent"
params = []

[[action.triggers]]
name = "start_job"
kind = "entity"
target_entity = "Job"
target_action = "Start"
resolve_target = { kind = "same_id" }
"#;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Unmade", "Started", "Done"]
initial = "Unmade"

[[action]]
name = "Start"
kind = "internal"
from = ["Unmade"]
to = "Started"
params = []

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Job"
target_action = "Finish"
resolve_target = { kind = "same_id" }

[[action]]
name = "Finish"
kind = "input"
from = ["Started"]
to = "Done"
params = []

[[liveness]]
name = "started_job_finishes"
eventually = "status != 'Started'"
"#;

fn check(policies: &str) -> JointLivenessResult {
    let ping = parse_automaton(PING).expect("fixture parses");
    let job = parse_automaton(JOB).expect("fixture parses");
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&ping, &job], Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    let result = results.remove(0);
    assert_eq!(result.scope, ["Job"], "{result}");
    result
}

#[test]
fn an_outside_reaction_runs_as_whoever_may_send_the_move_that_set_it_off() {
    // Only a member may Add a ping, and a member may Finish: every Start
    // that arrives is followed by a Finish Cedar allows.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping)
when { principal has role && principal.role == "member" };
permit(principal, action == Action::"Start", resource is Job)
when { context has via && context.via != "request" };
permit(principal, action == Action::"Finish", resource is Job)
when { principal has role && principal.role == "member" };
"#,
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn an_outside_reaction_cedar_refuses_its_sender_never_arrives() {
    // Anyone may Add a ping, but only a member may Start or Finish: a
    // stranger's ping is refused its Start, so no job starts unfinished.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping);
permit(principal, action in [Action::"Start", Action::"Finish"], resource is Job)
when { principal has role && principal.role == "member" };
"#,
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn a_stranger_who_starts_the_chain_outside_the_scope_is_refused_the_finish() {
    // Anyone may Add a ping and any reaction may Start, but only a member
    // may Finish: a stranger's job starts and stays Started.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping);
permit(principal, action == Action::"Start", resource is Job)
when { context has via && context.via != "request" };
permit(principal, action == Action::"Finish", resource is Job)
when { principal has role && principal.role == "member" };
"#,
    );
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}
