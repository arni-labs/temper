//! Entry rules: every move into a place must meet the rule as it fires.
//!
//! A guard on one move says when that move may fire, and a second move into
//! the same place need not repeat it. An invariant cannot say it either when
//! the condition may stop holding once the record is in the place. An entry
//! rule states it for every move in, and the cascade checks each one.

use temper_verify::{CascadeLevel, CascadeResult, VerificationCascade};

/// A cup that a barista may start making only while its order is paid and
/// its topping is in stock. Its topping may run out while it is being made,
/// and a wrong drink goes back to the same barista without a second check.
const CUP: &str = r#"
[automaton]
name = "Cup"
states = ["Unmade", "Waiting", "BeingMade", "Made", "Refunded"]
initial = "Unmade"

[[state]]
name = "ticket"
type = "string"
initial = ""

[[state]]
name = "topping"
type = "string"
initial = ""

[[state]]
name = "makers"
type = "counter"
initial = 0

[[action]]
name = "Add"
from = ["Unmade"]
to = "Waiting"

[[action]]
name = "Claim"
from = ["Waiting", "BeingMade"]
to = "BeingMade"
guard = "makers == 0 && Ticket[ticket].status == 'Paid' && Topping[topping].status == 'Stocked'"
effect = ["makers += 1"]

[[action]]
name = "Finish"
from = ["BeingMade"]
to = "Made"

[[action]]
name = "RefundCup"
from = ["Waiting"]
to = "Refunded"

[[action]]
name = "WrongDrink"
from = ["Made"]
to = "BeingMade"

[[invariant]]
name = "one_barista_per_cup"
assert = "makers <= 1"
"#;

const START_ONLY_IN_STOCK: &str = r#"
[[entry_rule]]
name = "start_only_in_stock"
place = "BeingMade"
from = ["Waiting"]
assert = "Topping[topping].status == 'Stocked'"
"#;

/// A second way to start the cup, which checks the order but not the stock.
const SWAP_AND_MAKE: &str = r#"
[[action]]
name = "SwapAndMake"
from = ["Waiting"]
to = "BeingMade"
guard = "makers == 0 && Ticket[ticket].status == 'Paid'"
effect = ["makers += 1"]
"#;

fn check(spec: &str) -> CascadeResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(100)
        .run()
}

/// The last move of L1's path to `rule`, and the place it ends in.
fn breaking_move(result: &CascadeResult, rule: &str) -> (String, String) {
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules" && c.violated == [rule])
        .unwrap_or_else(|| panic!("no path to {rule}: {}", l1.summary));
    let [.., (before, Some(action)), (after, None)] = found.trace.as_slice() else {
        panic!("the path ends without its move: {:?}", found.trace);
    };
    assert_eq!(before.status, "Waiting");
    (action.name.clone(), after.status.clone())
}

/// Swap and make keeps the one barista per cup, so no invariant breaks, but
/// it starts a cup without checking the stock: the rule names it.
#[test]
fn a_second_way_in_that_skips_the_rule_is_refused_with_its_path() {
    let result = check(&format!("{CUP}{SWAP_AND_MAKE}{START_ONLY_IN_STOCK}"));
    assert!(!result.all_passed);
    assert_eq!(
        breaking_move(&result, "start_only_in_stock"),
        ("SwapAndMake".to_string(), "BeingMade".to_string())
    );
    let l3 = result.level_result(CascadeLevel::PropertyTest).unwrap();
    assert!(
        !l3.passed && l3.summary.contains("start_only_in_stock"),
        "{}",
        l3.summary
    );
}

/// Without swap and make, the one way to start the cup checks the stock, and
/// a wrong drink goes back to being made from Made, a place the rule does
/// not cover.
#[test]
fn every_way_in_that_requires_the_rule_passes() {
    let result = check(&format!("{CUP}{START_ONLY_IN_STOCK}"));
    assert!(result.all_passed, "{result:?}");
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(l1.summary.contains("all properties hold"), "{}", l1.summary);
}

/// A rule that names no places covers every move in from another place, so
/// the wrong drink's way back in breaks it.
#[test]
fn a_rule_without_from_covers_every_other_place() {
    let rule = START_ONLY_IN_STOCK.replace("from = [\"Waiting\"]\n", "");
    let result = check(&format!("{CUP}{rule}"));
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .unwrap_or_else(|| panic!("the rule held: {}", l1.summary));
    let [.., (before, Some(action)), _] = found.trace.as_slice() else {
        panic!("the path ends without its move: {:?}", found.trace);
    };
    assert_eq!(
        (before.status.as_str(), action.name.as_str()),
        ("Made", "WrongDrink")
    );
}

#[test]
fn a_rule_no_move_is_checked_against_fails_to_load() {
    // Only Finish moves into Made, and it moves from BeingMade.
    let rule = START_ONLY_IN_STOCK.replace("place = \"BeingMade\"", "place = \"Made\"");
    let error = temper_spec::automaton::parse_automaton(&format!("{CUP}{rule}"))
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no action moves into 'Made' from a place it covers"),
        "{error}"
    );
}

#[test]
fn a_rule_may_not_read_a_move_s_parameters() {
    let rule = START_ONLY_IN_STOCK.replace(
        "Topping[topping].status == 'Stocked'",
        "params.topping != ''",
    );
    let error = temper_spec::automaton::parse_automaton(&format!("{CUP}{rule}"))
        .map(|_| ())
        .unwrap_err()
        .to_string();
    assert!(error.contains("params.topping"), "{error}");
}
