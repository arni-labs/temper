use temper_spec::automaton::parse_automaton;

use super::*;

/// A job finished once, with `done` set as it finishes, and three flags
/// set one after another, each only once the one before it is.
const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "done"
type = "bool"
initial = false

[[state]]
name = "a"
type = "bool"
initial = false

[[state]]
name = "b"
type = "bool"
initial = false

[[state]]
name = "c"
type = "bool"
initial = false

[[action]]
name = "Finish"
kind = "input"
from = ["Open"]
to = "Done"
effect = ["done = true"]

[[action]]
name = "SetA"
kind = "input"
from = ["Open"]
effect = ["a = true"]

[[action]]
name = "SetB"
kind = "input"
from = ["Open"]
guard = "a == true"
effect = ["b = true"]

[[action]]
name = "SetC"
kind = "input"
from = ["Open"]
guard = "b == true"
effect = ["c = true"]
"#;

fn proofs(invariants: &str) -> Vec<RuleProof> {
    let automaton = parse_automaton(&format!("{JOB}\n{invariants}")).expect("fixture parses");
    prove_alone(&automaton, None)
}

#[test]
fn an_invariant_every_move_keeps_is_proved() {
    let proofs = proofs(
        "[[invariant]]\nname = \"DoneIsSet\"\nassert = \"status == 'Done' => done == true\"\n",
    );
    assert_eq!(proofs.len(), 1);
    assert!(proofs[0].proved(), "{:?}", proofs[0].shortfall());
}

#[test]
fn a_true_invariant_its_moves_keep_only_with_a_lemma_is_not_proved() {
    // From a state with `b` and not `a`, which no run reaches, SetC breaks
    // it: the proof needs `b => a`, which nothing guesses.
    let proofs = proofs("[[invariant]]\nname = \"CNeedsA\"\nassert = \"c == true => a == true\"\n");
    assert_eq!(proofs.len(), 1);
    let shortfall = proofs[0].shortfall().expect("not proved");
    assert!(shortfall.contains("Job.SetC"), "{shortfall}");
}

#[test]
fn a_lemma_written_beside_the_invariant_proves_it() {
    let proofs = proofs(
        "[[invariant]]\nname = \"CNeedsA\"\nassert = \"c == true => a == true\"\n\n\
         [[invariant.lemma]]\nname = \"BNeedsA\"\nassert = \"(=> $Job.b $Job.a)\"\n",
    );
    assert!(proofs[0].proved(), "{:?}", proofs[0].shortfall());
}

#[test]
fn an_action_only_the_kernel_sends_may_come_at_any_time() {
    // The check of one type sends every action but an output itself, so
    // an internal one too: Close may come before Finish sets `done`.
    let spec = JOB.replace(
        "[[action]]\nname = \"SetA\"",
        "[[action]]\nname = \"Close\"\nkind = \"internal\"\nfrom = [\"Open\"]\nto = \"Done\"\n\n\
         [[action]]\nname = \"SetA\"",
    );
    let automaton = parse_automaton(&format!(
        "{spec}\n[[invariant]]\nname = \"DoneIsSet\"\nassert = \"status == 'Done' => done == true\"\n"
    ))
    .expect("fixture parses");
    let proofs = prove_alone(&automaton, None);
    let shortfall = proofs[0].shortfall().expect("not proved");
    assert!(shortfall.contains("Job.Close"), "{shortfall}");
}

#[test]
fn proofs_keep_declaration_indices_when_a_same_named_rule_reads_another_record() {
    let automaton = parse_automaton(&format!(
        r#"{JOB}
[[state]]
name = "parent_id"
type = "string"
initial = ""

[[invariant]]
name = "I"
assert = "Parent[parent_id].status == 'Done'"

[[invariant]]
name = "I"
assert = "status == 'Done' => done == true"
"#
    ))
    .expect("fixture parses");
    let proofs = prove_alone_indexed(&automaton, None);
    assert_eq!(proofs.len(), 1);
    assert_eq!(proofs[0].0, 1);
    assert!(proofs[0].1.proved(), "{:?}", proofs[0].1.shortfall());
}
