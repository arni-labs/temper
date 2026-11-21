//! A spawned child's first state is checked after its initial action, with
//! the initial action's guards reading the parent as live dispatch reads it:
//! committed, after the spawning move.

use super::*;

/// A child is never left `New`: its initial action, which needs its run
/// running, moves it on.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized"]
initial = "New"

[[action]]
name = "Join"
kind = "input"
from = ["New"]
to = "Initialized"
guard = "Run[run_id].status in ['Running']"

[[invariant]]
name = "NeverLeftNew"
assert = "status in ['Initialized']"
"#;

const RUN: &str = r#"
[automaton]
name = "Run"
states = ["Idle", "Running"]
initial = "Idle"

[[action]]
name = "Launch"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Join', ChildRef, params.child_id)"]
"#;

#[tokio::test]
async fn a_child_whose_initializer_reads_the_parent_status_is_created() {
    let state = build_state_with_specs(CHILD, RUN, "version = 1\n");
    let (status, body) = send(&state, Method::POST, "/tdata/Runs", json!({"Id": "r1"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed r1: {body}");

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.Launch",
        json!({"child_id": "c1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let tenant = TenantId::default();
    let mut child = None;
    for _ in 0..100 {
        if state.entity_exists(&tenant, "Child", "c1") {
            child = state
                .get_tenant_entity_state(&tenant, "Child", "c1")
                .await
                .ok();
            if child
                .as_ref()
                .is_some_and(|c| c.state.status == "Initialized")
            {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let child = child.expect("the spawn creates the child");
    assert_eq!(child.state.status, "Initialized");
}
