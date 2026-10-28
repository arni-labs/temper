//! A no-write parameter must satisfy every equality before a reaction lands.

use super::*;

const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "value"
type = "string"
initial = "b"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "seal"
kind = "entity"
principal = "tell"
target_entity = "Child"
target_action = "Seal"

[action.triggers.args]
sealed_with = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[liveness]]
name = "seals"
eventually = "status == 'Closed' && Child[Id].status == 'Done' => Child[Id].status == 'Sealed'"
relies_on = ["Close"]
"#;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Done", "Sealed"]
initial = "Done"

[[state]]
name = "a"
type = "string"
initial = ""

[[state]]
name = "b"
type = "string"
initial = "b"

[[action]]
name = "Seal"
kind = "internal"
from = ["Done"]
to = "Sealed"
params = [{ name = "sealed_with", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "sealed_with"
field = "a"

[[action.constraints]]
kind = "param_equals_field"
param = "sealed_with"
field = "b"

[[invariant]]
name = "seal_requires_equal_fields"
assert = "status == 'Sealed' => a == b && Parent[Id].status == 'Closed'"
"#;

#[test]
fn a_reaction_cannot_satisfy_conflicting_equalities_with_the_last_field() {
    let result = check_under(&[PARENT, CHILD], Some(ONLY_TELL_REACHES_THE_CHILD));
    assert!(result.holds(), "{result}");
}

#[test]
fn liveness_checks_every_equality_before_counting_on_a_reaction() {
    for (child, should_settle) in [
        (CHILD.to_string(), false),
        (CHILD.replace("initial = \"\"", "initial = \"b\""), true),
    ] {
        let automatons = parse(&[PARENT, &child]);
        let refs: Vec<&Automaton> = automatons.iter().collect();
        let analysis = SendAnalysis::new(ONLY_TELL_REACHES_THE_CHILD).unwrap();
        let mut results = verify_joint_liveness(
            &refs,
            Some(&analysis),
            Budget {
                time: Duration::from_secs(30),
                states: 100_000,
            },
        );
        assert_eq!(results.len(), 1);
        let result = results.remove(0);
        if should_settle {
            assert!(result.holds(), "matching equalities: {result}");
        } else {
            assert!(
                matches!(result.outcome, LivenessOutcome::Stuck { .. }),
                "{result}"
            );
        }
    }
}
