//! A budgeted cascade answers in bounded time, however large the spec's state
//! space, and says which type ran out and why.

use std::time::{Duration, Instant};

use temper_verify::{Budget, CascadeLevel, VerificationCascade};

/// A spec whose model check runs for minutes with no budget.
const HEAVY_FEED: &str = include_str!("../../../test-fixtures/budget/heavy_feed.toml");

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn budget(time: Duration) -> Budget {
    Budget {
        time,
        states: 5_000_000,
    }
}

#[test]
fn a_spec_too_large_to_check_stops_at_its_budget_and_names_itself() {
    let started = Instant::now();
    let result = VerificationCascade::from_ioa(HEAVY_FEED)
        .with_sim_seeds(5)
        .with_prop_test_cases(100)
        .with_budget(budget(Duration::from_secs(1)))
        .run();
    let took = started.elapsed();

    // The budget is 1 s; what follows it is draining a queue that no longer
    // grows. 10 s leaves room for a loaded CI machine, against the minutes
    // this spec takes with no budget.
    assert!(took < Duration::from_secs(10), "answered in {took:?}");
    assert!(!result.all_passed);
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped
            .starts_with("HeavyFeed: L1 Model Check STOPPED: not finished within its 1 s budget"),
        "{stopped}"
    );
    assert!(
        stopped.contains("fewer of them or smaller bounds"),
        "{stopped}"
    );
    let last = result.levels.last().expect("levels");
    assert_eq!(
        last.level,
        CascadeLevel::ModelCheck,
        "the levels after the cut do not run"
    );
    assert!(!last.passed);
    let verification = last.verification.as_ref().expect("model check result");
    assert!(!verification.is_complete);
}

/// Six counters each stepped by its own move: a million states at bound 9,
/// each cheap to step from.
const WIDE: &str = r#"
state = [
  { name = "a", type = "counter", initial = 0 },
  { name = "b", type = "counter", initial = 0 },
  { name = "c", type = "counter", initial = 0 },
  { name = "d", type = "counter", initial = 0 },
  { name = "e", type = "counter", initial = 0 },
  { name = "f", type = "counter", initial = 0 },
]
action = [
  { name = "A", kind = "input", from = ["On"], effect = ["a += 1"] },
  { name = "B", kind = "input", from = ["On"], effect = ["b += 1"] },
  { name = "C", kind = "input", from = ["On"], effect = ["c += 1"] },
  { name = "D", kind = "input", from = ["On"], effect = ["d += 1"] },
  { name = "E", kind = "input", from = ["On"], effect = ["e += 1"] },
  { name = "F", kind = "input", from = ["On"], effect = ["f += 1"] },
]

[automaton]
name = "Wide"
states = ["On"]
initial = "On"
"#;

#[test]
fn a_state_limit_stops_the_model_check_however_much_time_is_left() {
    let result = VerificationCascade::from_ioa(WIDE)
        .with_max_items(9)
        .with_budget(Budget {
            time: Duration::from_secs(600),
            states: 1_000,
        })
        .run();
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped.starts_with(
            "Wide: L1 Model Check STOPPED: reached its limit of 1000 generated states"
        ),
        "{stopped}"
    );
    assert_eq!(result.levels.len(), 2, "L0, then the model check it cut");
}

#[test]
fn a_spec_within_its_budget_is_checked_as_without_one() {
    let unbudgeted = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    let budgeted = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .with_budget(budget(Duration::from_secs(120)))
        .run();
    assert!(budgeted.all_passed && unbudgeted.all_passed);
    assert!(budgeted.stopped.is_none());
    let summaries = |r: &temper_verify::CascadeResult| -> Vec<String> {
        r.levels.iter().map(|l| l.summary.clone()).collect()
    };
    assert_eq!(summaries(&budgeted), summaries(&unbudgeted));
}
