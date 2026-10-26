//! Worker admission must also bound the threads waiting for a solver.

use super::*;

/// Number of worker threads created, observed with all seats reserved.
pub(super) static WORKERS_CREATED: AtomicUsize = AtomicUsize::new(0);

#[test]
fn saturated_solver_capacity_creates_no_waiting_workers() {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let reserved: Vec<_> = (0..cores).map(|_| Seat::take(None).unwrap()).collect();
    let before = WORKERS_CREATED.load(Ordering::SeqCst);
    let asks: Vec<_> = (0..64)
        .map(|i| Ask {
            name: i.to_string(),
            text: String::new(),
            goal: vec![],
            show: vec![],
            general: false,
            quantified: false,
        })
        .collect();
    let session = Session {
        prelude: "",
        few: "",
        clauses: "",
        literals: &[],
        solvers: 64,
        deadline: Some(Instant::now() + Duration::from_millis(50)),
    };
    let (obligations, errors) = run("saturated", &session, &asks, &BTreeSet::new()).unwrap();
    assert!(errors.is_empty());
    assert_eq!(obligations.len(), 1);
    assert!(matches!(obligations[0].outcome, Outcome::Unknown));
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
