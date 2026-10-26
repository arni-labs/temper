//! Worker admission must also bound the threads waiting for a solver.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use temper_spec::automaton::parse_automaton;

use super::super::hints::Hints;
use super::super::{Rule, prove_one};
use super::*;

/// Number of worker threads created, observed with all seats reserved.
pub(super) static WORKERS_CREATED: AtomicUsize = AtomicUsize::new(0);

const COUNTER: &str = r#"
[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "Increment"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["count += 1"]

[[invariant]]
name = "Nonnegative"
assert = "count >= 0"
"#;

#[test]
fn saturated_solver_capacity_creates_no_waiting_workers() {
    let automaton = parse_automaton(COUNTER).unwrap();
    let invariant = &automaton.invariants[0];
    let hints = Hints::given(
        &Hints::default(),
        &invariant.name,
        &invariant.lemma,
        &[],
        &[],
    );
    let answered = Mutex::default();
    let shared = Shared {
        by_name: BTreeMap::from([("Counter", &automaton)]),
        policies: None,
        answered: &answered,
        solvers: 64,
        deadline: Some(Instant::now() + Duration::from_millis(50)),
    };
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let reserved: Vec<_> = (0..cores).map(|_| Seat::take(None).unwrap()).collect();
    let before = WORKERS_CREATED.load(Ordering::SeqCst);
    let proof = prove_one(
        &shared,
        "Counter",
        &invariant.name,
        Rule::Always(&invariant.assert),
        &hints,
    )
    .unwrap();
    assert!(!proof.proved(), "{proof:?}");
    assert_eq!(
        WORKERS_CREATED.load(Ordering::SeqCst),
        before,
        "admission must precede thread creation"
    );
    drop(reserved);
}

#[test]
fn an_expired_deadline_admits_no_solver_even_when_capacity_is_free() {
    assert!(Seat::take(Some(Instant::now())).is_none());
}
