//! Dropped lemmas remain evidence when another move stops the batch.

use temper_spec::automaton::parse_automaton;

use super::*;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"

[[state]]
name = "safe"
type = "bool"
initial = true

[[state]]
name = "hint"
type = "bool"
initial = true

[[action]]
name = "BreakHint"
kind = "input"
from = ["Idle"]
effect = ["hint = false"]

[[action]]
name = "BreakRule"
kind = "input"
from = ["Idle"]
effect = ["safe = false"]

[[invariant]]
name = "is_safe"
assert = "safe == true"

[[invariant.lemma]]
name = "hint"
assert = "$Job.hint"
"#;

#[test]
fn a_broken_lemma_is_named_when_a_later_move_stops_the_batch() {
    let automaton = parse_automaton(JOB).unwrap();
    let invariant = &automaton.invariants[0];
    let hints = Hints::given(
        &Hints::default(),
        &invariant.name,
        &invariant.lemma,
        &[],
        &[],
    );
    let answered = Mutex::default();
    // One solver separates the checks; wider batches exercise early
    // termination. Control this independently of host cores.
    for solvers in [1, 2, 4] {
        let shared = Shared {
            by_name: BTreeMap::from([("Job", &automaton)]),
            policies: None,
            answered: &answered,
            solvers,
            deadline: None,
        };
        let proof = prove_one(
            &shared,
            "Job",
            &invariant.name,
            Rule::Always(&invariant.assert),
            &hints,
        )
        .unwrap();
        assert!(!proof.proved(), "{solvers} solvers: {proof:?}");
        assert!(
            proof.shortfall().unwrap().contains("Job.BreakRule"),
            "{solvers} solvers: {proof:?}"
        );
        assert_eq!(
            proof.dropped.get("hint").map(String::as_str),
            Some("fails after Job.BreakHint (from outside)"),
            "{solvers} solvers: {proof:?}"
        );
    }
}
