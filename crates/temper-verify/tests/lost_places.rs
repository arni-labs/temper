//! A record that can never reach a place another record in the same place
//! still can is named in L1's answer, as worth a look, never as a failure.
//!
//! No stated rule has to be broken: a cup sent back to the line with its
//! barista still counted is never made again, and every invariant holds.

use temper_verify::{CascadeLevel, VerificationCascade};

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

/// An order picked up in store or delivered: the choice closes off the
/// other way's places, on purpose.
const ORDER: &str = r#"
[automaton]
name = "Order"
states = ["Open", "Placed", "Ready", "PickedUp", "OutForDelivery", "Delivered"]
initial = "Open"

[[state]]
name = "delivery"
type = "counter"
initial = 0

[[action]]
name = "Pickup"
from = ["Open"]
to = "Placed"

[[action]]
name = "Delivery"
from = ["Open"]
to = "Placed"
effect = ["delivery = 1"]

[[action]]
name = "AllMade"
from = ["Placed"]
to = "Ready"

[[action]]
name = "HandOver"
from = ["Ready"]
to = "PickedUp"
guard = "delivery == 0"

[[action]]
name = "TakeOut"
from = ["Ready"]
to = "OutForDelivery"
guard = "delivery == 1"

[[action]]
name = "Drop"
from = ["OutForDelivery"]
to = "Delivered"
"#;

const TICKET: &str = include_str!("../../../test-fixtures/specs/ticket.ioa.toml");

fn model_check(spec: &str) -> (bool, String) {
    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(50)
        .run();
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    (result.all_passed, l1.summary.clone())
}

#[test]
fn a_cup_sent_back_that_can_never_be_made_again_is_named_but_passes() {
    let (passed, summary) = model_check(CUP);
    assert!(passed, "{summary}");
    assert!(
        summary.starts_with("L1 Model Check PASSED: 7 states explored, all properties hold. "),
        "{summary}"
    );
    assert!(
        summary.contains(
            "Not a failure, but worth a look: a record in Waiting with makers = 1 can never \
             reach BeingMade or Made, while one with makers = 0 can (the first after Add > \
             Claim > Finish > WrongDrink, the second after Add). If that is intended, nothing \
             to do."
        ),
        "{summary}"
    );
}

/// Sent back to be made again, the cup keeps its barista and every cup in a
/// place can reach what any other can.
#[test]
fn a_cup_sent_back_to_be_made_again_is_not_named() {
    let spec = CUP.replace(
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"Waiting\"",
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"BeingMade\"",
    );
    assert!(spec.contains("to = \"BeingMade\"\n\n[[invariant]]"));
    let (passed, summary) = model_check(&spec);
    assert!(passed, "{summary}");
    assert_eq!(
        summary,
        "L1 Model Check PASSED: 5 states explored, all properties hold"
    );
}

/// The choice is named once, where it is made, not again in each place the
/// order passes through after it.
#[test]
fn a_choice_that_closes_off_places_is_named_where_it_is_made() {
    let (passed, summary) = model_check(ORDER);
    assert!(passed, "{summary}");
    assert!(
        summary.contains(
            "a record in Placed with delivery = 1 can never reach PickedUp, while one with \
             delivery = 0 can (the first after Delivery, the second after Pickup)"
        ),
        "{summary}"
    );
    assert!(
        summary.contains(
            "a record in Placed with delivery = 0 can never reach OutForDelivery or Delivered, \
             while one with delivery = 1 can (the first after Pickup, the second after Delivery)"
        ),
        "{summary}"
    );
    assert_eq!(summary.matches("can never reach").count(), 2, "{summary}");
}

/// The ticket's replies stop at their bound: a reply past it is not walked,
/// so what a record with that many replies can reach is not known.
#[test]
fn a_record_whose_future_goes_past_a_bound_is_not_named() {
    let (_, summary) = model_check(TICKET);
    assert!(summary.contains("bounded"), "{summary}");
    assert!(!summary.contains("can never reach"), "{summary}");
}
