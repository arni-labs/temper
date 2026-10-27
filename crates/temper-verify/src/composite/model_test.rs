//! Tests for the composite joint-state model (ADR-0046 / ADR-0150).

use super::*;
use crate::composite::CompositeVerificationPlan;
use stateright::Checker;
use temper_spec::automaton::parse_automaton;

fn order_ioa() -> &'static str {
    r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Confirmed"]

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "auth_payment"
kind = "entity"
target_entity = "Payment"
target_action = "AuthorizePayment"
# Payment may be authorized independently before the Order's reaction fires;
# in this toy model that is benign convergence, so the reaction is best-effort.
drop_ok = true

[action.triggers.resolve_target]
kind = "same_id"
"#
}

fn payment_ioa() -> &'static str {
    r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Authorized"]

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
}

#[test]
fn bfs_explores_joint_state_space_with_cascade() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
    let model = CompositeTemperModel::from_plan(plan);

    let init_states = model.init_states();
    assert_eq!(init_states.len(), 1);
    let init = &init_states[0];
    assert_eq!(init.entities.get("Order").unwrap().status, "Draft");
    assert_eq!(init.entities.get("Payment").unwrap().status, "Pending");

    // Fire ConfirmOrder — cascade should advance Payment to Authorized.
    let mut actions = Vec::new();
    model.actions(init, &mut actions);
    let confirm = actions
        .iter()
        .find(|a| a.entity == "Order" && a.action.name == "ConfirmOrder")
        .expect("Order.ConfirmOrder enabled from Draft");
    let after = model.next_state(init, confirm.clone()).unwrap();
    assert_eq!(after.entities.get("Order").unwrap().status, "Confirmed");
    assert_eq!(
        after.entities.get("Payment").unwrap().status,
        "Authorized",
        "cascade should have triggered Payment.AuthorizePayment"
    );
}

#[test]
fn bfs_checker_proves_joint_invariant_holds() {
    // Run Stateright BFS over the composite model — ensures no
    // joint state violates the local invariants.
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    let checker = model.checker().spawn_bfs().join();
    // "always" properties emit no discoveries when they hold.
    let discoveries = checker.discoveries();
    assert!(
        discoveries.is_empty(),
        "unexpected discoveries: {discoveries:?}"
    );
    // Ensure BFS actually visited multiple states.
    assert!(checker.unique_state_count() >= 2);
}

#[test]
fn self_triggering_cascade_runs_once() {
    // Self-triggering entity (Assign → Start on same entity) should
    // not blow up the state space; the cascade ends after Start.
    let spec = r#"
[automaton]
name = "Agent"
states = ["Idle", "Assigned", "Working"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Assigned", "Working"]

[[action]]
name = "Assign"
from = ["Idle"]
to = "Assigned"

[[action.triggers]]
name = "auto_start"
kind = "entity"
target_entity = "Agent"
target_action = "Start"
guard = "status == 'Assigned'"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Start"
from = ["Assigned"]
to = "Working"
"#;
    let agent = parse_automaton(spec).unwrap();
    let plan = CompositeVerificationPlan::new(&[&agent], "Agent").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    let init = &model.init_states()[0];
    let mut actions = Vec::new();
    model.actions(init, &mut actions);
    let assign = actions
        .iter()
        .find(|a| a.action.name == "Assign")
        .cloned()
        .unwrap();
    let after = model.next_state(init, assign).unwrap();
    // The inline cascade fires Start automatically once — Agent lands in Working.
    assert_eq!(after.entities.get("Agent").unwrap().status, "Working");
}

/// A `Steps` spec whose action `A{i}` moves it from `S{i}` to `S{i+1}` and
/// triggers the next action on itself, so firing `A0` sets off a cascade of
/// `actions` distinct steps.
fn stepping_ioa(actions: usize) -> String {
    let states = (0..=actions)
        .map(|index| format!("\"S{index}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let mut ioa = format!(
        "[automaton]\nname = \"Steps\"\nstates = [{states}]\ninitial = \"S0\"\nallow_indefinite_states = [{states}]\n"
    );
    for index in 0..actions {
        let next = index + 1;
        ioa.push_str(&format!(
            "\n[[action]]\nname = \"A{index}\"\nfrom = [\"S{index}\"]\nto = \"S{next}\"\n"
        ));
        if next < actions {
            ioa.push_str(&format!(
                "\n[[action.triggers]]\nname = \"then_a{next}\"\nkind = \"entity\"\ntarget_entity = \"Steps\"\ntarget_action = \"A{next}\"\n\n[action.triggers.resolve_target]\nkind = \"same_id\"\n"
            ));
        }
    }
    ioa
}

#[test]
fn a_long_cascade_of_distinct_steps_runs_to_its_end_on_a_small_thread() {
    // A cascade stops only at a repeated step, so one of distinct steps runs
    // as far as the spec goes, however small the checking thread's stack. A
    // thread that overflows its stack aborts the process, so this fails by
    // crashing.
    const ACTIONS: usize = 2_000;
    let steps = parse_automaton(&stepping_ioa(ACTIONS)).unwrap();
    let plan = CompositeVerificationPlan::new(&[&steps], "Steps").unwrap();
    let model = CompositeTemperModel::from_plan(plan);
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_scoped(scope, || {
                let init = &model.init_states()[0];
                let mut actions = Vec::new();
                model.actions(init, &mut actions);
                let first = actions
                    .iter()
                    .find(|a| a.action.name == "A0")
                    .cloned()
                    .unwrap();
                let after = model.next_state(init, first).unwrap();
                assert_eq!(
                    after.entities.get("Steps").unwrap().status,
                    format!("S{ACTIONS}")
                );
            })
            .expect("spawn the test thread")
            .join()
            .expect("the cascade ran to its end");
    });
}

#[test]
fn a_created_child_runs_before_the_next_made_step_repeats() {
    for (resolver, child_action, child_id, expected_runs) in [
        ("same_id", "Sprout", "", 1),
        ("create", "Sprout", "", 2),
        ("create", "Grow", "", 2),
        ("create_if_missing", "Sprout", "", 2),
        ("create_if_missing", "Grow", "", 2),
        ("create_if_missing", "Sprout", "original", 1),
        ("create_if_missing", "Grow", "original", 1),
    ] {
        let id_field = if resolver == "create_if_missing" {
            r#", id_field = "child_id""#
        } else {
            ""
        };
        let seed = parse_automaton(&format!(
            r#"
[automaton]
name = "Seed"
states = ["Sown"]
initial = "Sown"
allow_indefinite_states = ["Sown"]

[[state]]
name = "child_id"
type = "string"
initial = "{child_id}"

[[state]]
name = "sprouts"
type = "counter"
initial = 0

[[action]]
name = "Sprout"
from = ["Sown"]
to = "Sown"
effect = ["sprouts += 1"]

[[action.triggers]]
name = "sprout_again"
kind = "entity"
target_entity = "Seed"
target_action = "{child_action}"
resolve_target = {{ kind = "{resolver}"{id_field} }}

[[action.triggers]]
name = "record_sprout"
kind = "entity"
target_entity = "Observer"
target_action = "Record"
resolve_target = {{ kind = "same_id" }}

[[action]]
name = "Grow"
from = ["Sown"]
to = "Sown"

[[action.triggers]]
name = "sprout_on_child"
kind = "entity"
target_entity = "Seed"
target_action = "Sprout"
resolve_target = {{ kind = "same_id" }}
"#
        ))
        .unwrap();
        let observer = parse_automaton(
            r#"
[automaton]
name = "Observer"
states = ["Watching"]
initial = "Watching"
allow_indefinite_states = ["Watching"]

[[state]]
name = "records"
type = "counter"
initial = 0

[[action]]
name = "Record"
from = ["Watching"]
to = "Watching"
effect = ["records += 1"]
"#,
        )
        .unwrap();
        let plan = CompositeVerificationPlan::new(&[&seed, &observer], "Seed").unwrap();
        let model = CompositeTemperModel::from_plan(plan);
        let init = &model.init_states()[0];
        let mut actions = Vec::new();
        model.actions(init, &mut actions);
        let sprout = actions
            .into_iter()
            .find(|a| a.entity == "Seed" && a.action.name == "Sprout")
            .unwrap();
        let after = model.next_state(init, sprout).unwrap();

        // The joint model accumulates effects in one slice per type. A found
        // step runs once; a create admits one made step too, then stops the
        // next made step. Both admitted actions must fire their reactions.
        assert_eq!(
            after.entities["Seed"].counters["sprouts"], expected_runs,
            "{resolver} {child_action} with id {child_id:?}: effects of admitted Sprout actions"
        );
        assert_eq!(
            after.entities["Observer"].counters["records"], expected_runs,
            "{resolver} {child_action} with id {child_id:?}: downstream reactions of admitted Sprout actions"
        );
        assert!(after.dropped.is_none());
    }
}
