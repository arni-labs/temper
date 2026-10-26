//! Concrete call answers must be possible in the proof as in the kernel.

use super::encode::prelude;
use super::moves::Step;
use super::solver::{Answer, Z3};
use super::*;
use temper_spec::automaton::parse_automaton;

fn source(kind: &str) -> String {
    format!(
        r#"
[automaton]
name = "Job"
states = ["Idle", "Running", "Updated", "Finished"]
initial = "Idle"
[[state]]
name = "commit"
type = "string"
initial = "old"
[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
[[action.triggers]]
name = "work"
kind = "{kind}"
{kind} = "worker"
on_success = "Finish"
[[action]]
name = "Finish"
kind = "input"
from = ["Updated"]
to = "Finished"
params = [{{ name = "commit", type = "string" }}]
[[action.constraints]]
kind = "param_equals_field"
param = "commit"
field = "commit"
"#
    )
    .replace("wasm =", "module =")
}

#[test]
fn a_proof_allows_returned_parameters_to_differ_from_call_time_fields() {
    for kind in ["wasm", "adapter"] {
        let automaton = parse_automaton(&source(kind)).unwrap();
        let answered = Mutex::default();
        let v = Vocab::new(
            BTreeMap::from([("Job", &automaton)]),
            BTreeSet::from(["Job".to_string()]),
            None,
            &answered,
        );
        let enc = Enc::new(&v);
        let moves = Moves::new(&enc).all();
        let answer = moves
            .iter()
            .find(|m| m.step == Step::Answer && m.action == "Finish")
            .unwrap();
        let flight = v
            .flights()
            .into_iter()
            .find(|f| Some(f.name) == answer.witness.as_deref())
            .unwrap();
        // The call began with 'old'. If the encoding saves any fields, give
        // that witness the old values. The record now holds 'new'. A reply
        // choosing 'new' satisfies the kernel's equality constraint.
        let message = if flight.fields.is_empty() {
            format!("mk/{}", flight.name)
        } else {
            let values = flight
                .fields
                .iter()
                .map(|_| Enc::lit("old"))
                .collect::<Vec<_>>();
            format!("(mk/{} {})", flight.name, values.join(" "))
        };
        let mut z3 = Z3::start().unwrap();
        z3.send(&prelude(
            &v,
            &BTreeSet::from(["old".to_string(), "new".to_string()]),
        ))
        .unwrap();
        z3.send(&answer.decls.join("\n")).unwrap();
        z3.send(&format!("(assert (= w {message}))\n(assert (= pre.Job.status Job/Updated))\n(assert (= pre.Job.commit {}))", Enc::lit("new"))).unwrap();
        for formula in answer.body() {
            z3.send(&format!("(assert {formula})")).unwrap();
        }
        assert_eq!(
            z3.check().unwrap().0,
            Answer::Sat,
            "{kind}: {:?}",
            z3.errors
        );
        assert!(z3.errors.is_empty(), "{:?}", z3.errors);
    }
}

#[test]
fn proof_sender_checks_keep_the_adapter_background_identity() {
    for (kind, expected) in [("adapter", May::Yes), ("wasm", May::No)] {
        let automaton = parse_automaton(&source(kind)).unwrap();
        let policies = SendAnalysis::new(r#"permit(principal == Agent::"service:platform-dispatch", action == Action::"Poison", resource);"#).unwrap();
        let answered = Mutex::default();
        let v = Vocab::new(
            BTreeMap::from([("Job", &automaton)]),
            BTreeSet::from(["Job".to_string()]),
            Some(&policies),
            &answered,
        );
        assert_eq!(
            v.scheduled_may("Job", "Finish", "Poison"),
            expected,
            "{kind}"
        );
    }
}
