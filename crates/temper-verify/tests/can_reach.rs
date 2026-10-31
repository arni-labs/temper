//! A `[[liveness]]` rule with `can_reach` says a record in its `from` places
//! can always still reach one of the places it names: some moves lead there,
//! however the record got where it is. L1 refuses a record that never can,
//! with the path to it.
//!
//! Unlike `reaches`, the rule does not ask that every path get there, only
//! that the way is never closed off.

use temper_spec::automaton::{LivenessEnforcement, parse_automaton_with_liveness};
use temper_verify::{CascadeLevel, LevelResult, VerificationCascade};

/// A cup made by one barista. A wrong drink goes back to the line, but
/// still counts its barista, so no barista may claim it again.
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
from = ["Waiting"]
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
to = "Waiting"

[[invariant]]
name = "one_barista_per_cup"
assert = "makers <= 1"
"#;

/// A cup on the line can always still be made.
const CAN_BE_MADE: &str = r#"
[[liveness]]
name = "can_still_be_made"
from = ["Waiting"]
can_reach = ["Made"]
"#;

const TICKET: &str = include_str!("../../../test-fixtures/specs/ticket.ioa.toml");

fn model_check(spec: &str) -> LevelResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(50)
        .run()
        .level_result(CascadeLevel::ModelCheck)
        .cloned()
        .expect("L1 ran")
}

/// The moves of the path L1 gives for the rule `name`, and its last state's
/// place and makers.
fn broken(l1: &LevelResult, name: &str) -> (Vec<String>, String, usize) {
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.property == "CanReach" && c.violated == [name])
        .unwrap_or_else(|| panic!("no path for {name}: {}", l1.summary));
    let moves = found
        .trace
        .iter()
        .filter_map(|(_, action)| action.as_ref().map(|a| a.name.clone()))
        .collect();
    let (last, action) = found.trace.last().unwrap();
    assert!(action.is_none(), "the path ends at the record");
    (
        moves,
        last.status.clone(),
        last.counters.get("makers").copied().unwrap_or_default(),
    )
}

#[test]
fn a_cup_sent_back_that_can_never_be_made_again_breaks_the_rule() {
    let l1 = model_check(&format!("{CUP}{CAN_BE_MADE}"));
    assert!(
        l1.summary
            .starts_with("L1 Model Check FAILED: 7 states explored, 1 counterexample(s)"),
        "{}",
        l1.summary
    );
    let (moves, place, makers) = broken(&l1, "can_still_be_made");
    assert_eq!(moves, ["Add", "Claim", "Finish", "WrongDrink"]);
    assert_eq!((place.as_str(), makers), ("Waiting", 1));
    assert!(
        l1.diagnostics.iter().any(|line| line
            == "rule 'can_still_be_made' broken: the record at the end of this path can never \
                reach Made (4 step(s)):"),
        "{:?}",
        l1.diagnostics
    );
}

/// Sent back to be made again, the cup keeps its barista and can always
/// still be made.
#[test]
fn a_cup_sent_back_to_be_made_again_keeps_the_rule() {
    let spec = CUP.replace(
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"Waiting\"",
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"BeingMade\"",
    );
    assert!(spec.contains("to = \"BeingMade\"\n\n[[invariant]]"));
    let l1 = model_check(&format!("{spec}{CAN_BE_MADE}"));
    assert!(l1.passed, "{}", l1.summary);
    assert_eq!(
        l1.summary,
        "L1 Model Check PASSED: 5 states explored, all properties hold"
    );
}

/// With no `from`, the rule holds in every place, and the path is the
/// shortest to a record that breaks it: a refunded cup is never made.
#[test]
fn a_rule_with_no_from_holds_in_every_place() {
    let spec = CUP.replace(
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"Waiting\"",
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"BeingMade\"",
    );
    let l1 = model_check(&format!(
        "{spec}\n[[liveness]]\nname = \"always_made\"\ncan_reach = [\"Made\"]\n"
    ));
    let (moves, place, _) = broken(&l1, "always_made");
    assert_eq!(moves, ["Add", "RefundCup"]);
    assert_eq!(place, "Refunded");
}

/// A ticket with as many replies as the bound lets it have can't reply
/// again within the walk. The reply past the bound was not walked, so the
/// rule holds as far as the walk went: a bounded pass, not a refusal.
#[test]
fn a_record_whose_future_goes_past_a_bound_gives_a_bounded_pass() {
    let l1 = model_check(&format!(
        "{TICKET}\n[[liveness]]\nname = \"can_always_reply\"\nfrom = [\"InProgress\"]\n\
         can_reach = [\"WaitingOnCustomer\"]\n"
    ));
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
}

fn load(rule: &str) -> String {
    let spec = format!("{CUP}\n[[liveness]]\nname = \"rule\"\n{rule}\n");
    parse_automaton_with_liveness(&spec, LivenessEnforcement::WarnOnly)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

#[test]
fn a_can_reach_rule_that_also_says_reaches_or_has_actions_fails_to_load() {
    for rule in [
        "can_reach = [\"Made\"]\nreaches = [\"Made\"]",
        "can_reach = [\"Made\"]\nhas_actions = true",
    ] {
        let error = load(rule);
        assert!(
            error.contains("liveness 'rule': `can_reach` takes no `reaches` or `has_actions`"),
            "{rule}: {error}"
        );
    }
    let error = load("can_reach = [\"Made\"]\neventually = \"makers == 0\"");
    assert!(
        error.contains(
            "liveness 'rule': `eventually` takes no `from`, `reaches`, `can_reach` or `has_actions`"
        ),
        "{error}"
    );
}

/// A misspelt `from` would leave the rule checking nothing, and a misspelt
/// place in `can_reach` would refuse every record for a place none can be in.
#[test]
fn a_can_reach_rule_naming_an_undeclared_place_fails_to_load() {
    let error = load("from = [\"Wating\"]\ncan_reach = [\"Made\"]");
    assert!(
        error.contains("liveness 'rule': `from` names no place 'Wating'"),
        "{error}"
    );
    let error = load("from = [\"Waiting\"]\ncan_reach = [\"Mde\"]");
    assert!(
        error.contains("liveness 'rule': `can_reach` names no place 'Mde'"),
        "{error}"
    );
    assert_eq!(load("from = [\"Waiting\"]\ncan_reach = [\"Made\"]"), "");
}
