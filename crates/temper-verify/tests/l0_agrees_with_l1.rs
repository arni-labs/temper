//! L0 never calls an invariant inductive that the model check (L1) can break.
//!
//! L0 proves invariants by induction; L1 walks every reachable state. An
//! invariant that holds initially and that no transition can break from a
//! state where it holds is true in every reachable state, so a proof L1 can
//! contradict is a wrong proof. The Getaway film caught one: L0 reported
//! `tickets_home <= 1` inductive while L1 found a path to `tickets_home = 2`.

use proptest::prelude::*;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{
    CascadeLevel, VerificationCascade, check_model, run_prop_tests_from_ioa, verify_symbolic,
};

const TRIP_REBOOKING: &str = include_str!("../../../test-fixtures/specs/trip_rebooking.ioa.toml");

/// The film's failure, through the cascade: L0 does not call the rule
/// inductive, and L1 names it and shows the whole path to the move that
/// breaks it.
#[test]
fn the_cascade_names_the_rule_l1_breaks_and_the_path_to_it() {
    let result = VerificationCascade::from_ioa(TRIP_REBOOKING)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();

    let l0 = result
        .level_result(CascadeLevel::SymbolicVerification)
        .expect("L0 ran");
    assert!(l0.passed, "{}", l0.summary);
    assert!(
        l0.summary.contains("1 of 2 invariants proved inductive")
            && l0
                .summary
                .contains("not proved, left to the model check: NeverPaysForTwoFlightsHome (Bought can break it)"),
        "{}",
        l0.summary
    );

    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(!l1.passed);
    let header = &l1.diagnostics[0];
    assert!(
        header.contains("(NeverPaysForTwoFlightsHome false)"),
        "{header}"
    );
    let path: Vec<&String> = l1
        .diagnostics
        .iter()
        .skip(1)
        .take_while(|line| line.starts_with("    "))
        .collect();
    let steps = l1.verification.as_ref().expect("L1 detail").counterexamples[0]
        .trace
        .len();
    assert_eq!(path.len(), steps, "every step is shown: {path:?}");
    assert!(path[0].contains("0. Away"), "{path:?}");
    assert!(
        path[path.len() - 2].contains("--Bought"),
        "the path ends at the move that breaks the rule: {path:?}"
    );
    assert!(path[path.len() - 1].contains("tickets_home=2"), "{path:?}");
}

/// The counterexample's structured trace ends at the first state that breaks
/// the rule, where it used to run on to the last such state Stateright
/// visited (here: on to `FlyHome` and `Home`).
#[test]
fn a_counterexample_ends_where_the_rule_breaks() {
    let model = build_model_from_ioa(TRIP_REBOOKING, 2).expect("spec builds");
    let result = check_model(&model);
    let violation = result
        .counterexamples
        .iter()
        .find(|c| c.property == "Invariants")
        .expect("L1 breaks the rule");
    assert_eq!(violation.violated, ["NeverPaysForTwoFlightsHome"]);

    let (last, after) = violation.trace.last().expect("a path");
    assert!(after.is_none());
    assert_eq!(last.counters["tickets_home"], 2);
    let (before, bought) = &violation.trace[violation.trace.len() - 2];
    assert_eq!(before.counters["tickets_home"], 1);
    assert_eq!(bought.as_ref().map(|a| a.name.as_str()), Some("Bought"));
    assert!(
        violation.trace[..violation.trace.len() - 1]
            .iter()
            .all(|(state, _)| state.counters["tickets_home"] <= 1),
        "every earlier state keeps the rule"
    );
}

/// L3's failure carries the same per-step trace as L1's, so an agent has a
/// whole path even when L1 stops on its budget before finding one.
#[test]
fn a_property_test_failure_carries_every_state_on_its_path() {
    // Mode 1 is deterministic: its walks are seeded by the case number.
    let result = run_prop_tests_from_ioa(TRIP_REBOOKING, 500, 30);
    let failure = result.failure.expect("a walk pays for two flights home");
    assert_eq!(failure.invariant, "NeverPaysForTwoFlightsHome");
    assert_eq!(failure.trace.len(), failure.action_sequence.len() + 1);
    let (last, after) = failure.trace.last().expect("a path");
    assert!(after.is_none());
    assert_eq!(last.counters["tickets_home"], 2);
    let (_, bought) = &failure.trace[failure.trace.len() - 2];
    assert_eq!(bought.as_ref().map(|a| a.name.as_str()), Some("Bought"));
}

// ---------------------------------------------------------------------------
// Generated specs
// ---------------------------------------------------------------------------

const STATES: [&str; 3] = ["S0", "S1", "S2"];

fn counter_atom() -> impl Strategy<Value = String> {
    (
        0..2usize,
        prop::sample::select(vec!["==", "!=", "<", "<=", ">", ">="]),
        0..=3i64,
    )
        .prop_map(|(c, op, n)| format!("c{c} {op} {n}"))
}

fn bool_atom() -> impl Strategy<Value = String> {
    (0..2usize, any::<bool>()).prop_map(|(b, negated)| {
        if negated {
            format!("!b{b}")
        } else {
            format!("b{b}")
        }
    })
}

fn status_atom() -> impl Strategy<Value = String> {
    prop::sample::subsequence(STATES.to_vec(), 1..=2).prop_map(|states| {
        let quoted: Vec<String> = states.iter().map(|s| format!("'{s}'")).collect();
        format!("status in [{}]", quoted.join(", "))
    })
}

fn local_atom() -> impl Strategy<Value = String> {
    prop_oneof![counter_atom(), bool_atom()]
}

/// A guard: a condition on counters and flags, or none.
fn guard() -> impl Strategy<Value = Option<String>> {
    prop::option::of(prop_oneof![
        local_atom(),
        (local_atom(), local_atom()).prop_map(|(a, b)| format!("{a} && {b}")),
        (local_atom(), local_atom()).prop_map(|(a, b)| format!("{a} || {b}")),
    ])
}

fn any_atom() -> impl Strategy<Value = String> {
    prop_oneof![local_atom(), status_atom()]
}

/// An invariant, with or without a status trigger.
fn invariant() -> impl Strategy<Value = String> {
    prop_oneof![
        any_atom(),
        (any_atom(), any_atom()).prop_map(|(a, b)| format!("{a} && {b}")),
        (any_atom(), any_atom()).prop_map(|(a, b)| format!("{a} || {b}")),
        (status_atom(), local_atom()).prop_map(|(s, a)| format!("{s} => {a}")),
    ]
}

/// One effect statement, as `(variable written, statement)`.
fn effect() -> impl Strategy<Value = (String, String)> {
    prop_oneof![
        (0..2usize).prop_map(|c| (format!("c{c}"), format!("c{c} += 1"))),
        (0..2usize).prop_map(|c| (format!("c{c}"), format!("c{c} -= 1"))),
        (0..2usize, 0..=2i64).prop_map(|(c, n)| (format!("c{c}"), format!("c{c} = {n}"))),
        (0..2usize, any::<bool>()).prop_map(|(b, v)| (format!("b{b}"), format!("b{b} = {v}"))),
        (0..2usize).prop_map(|b| (format!("b{b}"), format!("b{b} = b{}", 1 - b))),
    ]
}

/// Effects writing distinct variables.
fn effects() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(effect(), 0..=2).prop_map(|effects| {
        let mut written = Vec::new();
        let mut kept = Vec::new();
        for (var, statement) in effects {
            if !written.contains(&var) {
                written.push(var);
                kept.push(statement);
            }
        }
        kept
    })
}

fn action() -> impl Strategy<
    Value = (
        Vec<&'static str>,
        Option<&'static str>,
        Option<String>,
        Vec<String>,
    ),
> {
    (
        prop::sample::subsequence(STATES.to_vec(), 1..=3),
        prop::option::of(prop::sample::select(STATES.to_vec())),
        guard(),
        effects(),
    )
}

fn quoted(items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", items.join(", "))
}

fn spec() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(0..=1usize, 2),
        prop::collection::vec(any::<bool>(), 2),
        prop::collection::vec(action(), 1..=5),
        prop::collection::vec(invariant(), 1..=2),
    )
        .prop_map(|(counters, bools, actions, invariants)| {
            let mut toml = format!(
                "[automaton]\nname = \"Gen\"\nstates = {states}\ninitial = \"S0\"\nallow_indefinite_states = {states}\n",
                states = quoted(&STATES)
            );
            for (i, initial) in counters.iter().enumerate() {
                toml.push_str(&format!(
                    "\n[[state]]\nname = \"c{i}\"\ntype = \"counter\"\ninitial = {initial}\n"
                ));
            }
            for (i, initial) in bools.iter().enumerate() {
                toml.push_str(&format!(
                    "\n[[state]]\nname = \"b{i}\"\ntype = \"bool\"\ninitial = {initial}\n"
                ));
            }
            for (i, (from, to, guard, effects)) in actions.iter().enumerate() {
                toml.push_str(&format!("\n[[action]]\nname = \"A{i}\"\nfrom = {}\n", quoted(from)));
                if let Some(to) = to {
                    toml.push_str(&format!("to = \"{to}\"\n"));
                }
                if let Some(guard) = guard {
                    toml.push_str(&format!("guard = \"{guard}\"\n"));
                }
                if !effects.is_empty() {
                    let effects: Vec<&str> = effects.iter().map(String::as_str).collect();
                    toml.push_str(&format!("effect = {}\n", quoted(&effects)));
                }
            }
            for (i, assert) in invariants.iter().enumerate() {
                toml.push_str(&format!(
                    "\n[[invariant]]\nname = \"I{i}\"\nassert = \"{assert}\"\n"
                ));
            }
            toml
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For every invariant L0 proves, the model check of that invariant alone
    /// (so another invariant's violation cannot hide it) finds no violation.
    #[test]
    fn l0_never_proves_an_invariant_l1_breaks(spec in spec()) {
        let model = build_model_from_ioa(&spec, 2);
        prop_assert!(
            model.is_ok(),
            "generated spec does not build: {:?}\n{spec}",
            model.as_ref().err()
        );
        let model = model.unwrap();
        let symbolic = verify_symbolic(&spec, 2);
        for inv in &model.invariants {
            let proved = symbolic
                .inductive_invariants
                .iter()
                .any(|(name, proved)| name == &inv.name && *proved);
            if !proved {
                continue;
            }
            let mut alone = model.clone();
            alone.invariants.retain(|other| other.name == inv.name);
            let checked = check_model(&alone);
            prop_assert!(checked.is_complete);
            let broken = checked
                .counterexamples
                .iter()
                .find(|c| c.property == "Invariants");
            prop_assert!(
                broken.is_none(),
                "L0 proved {} but L1 breaks it: {:?}\n{spec}",
                inv.name,
                broken
            );
        }
    }
}
