//! A call's answer is what the kernel dispatches: its trigger's `on_success`
//! when it names one, and otherwise any action of its record that the call's
//! result names. Cedar decides neither. The proof and the walk both take
//! exactly those answers.

use std::time::Duration;

use temper_authz::SendAnalysis;
use temper_spec::automaton::parse_automaton;

use super::*;

/// A job someone starts, which calls a worker through a trigger of `kind`,
/// with `answer` as its `on_success` when given. Nobody outside may send
/// `Finish` or `Drop` (see [`STARTS`]). `rule` is the job's invariant.
fn job(kind: &str, answer: Option<&str>, rule: &str) -> String {
    let named = answer
        .map(|answer| format!("on_success = \"{answer}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
[automaton]
name = "Job"
states = ["Idle", "Running", "Finished", "Dropped"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"

[[action.triggers]]
name = "work"
{kind}
{named}
[[action]]
name = "Finish"
kind = "input"
from = ["Running"]
to = "Finished"

[[action]]
name = "Drop"
kind = "input"
from = ["Running"]
to = "Dropped"

[[invariant]]
name = "Rule"
assert = "{rule}"
"#
    )
}

const WASM: &str = "kind = \"wasm\"\nmodule = \"worker\"";
const ADAPTER: &str = "kind = \"adapter\"\nadapter = \"worker\"";

/// Only `Start` may be sent from outside.
const STARTS: &str = r#"permit(principal, action == Action::"Start", resource is Job);"#;

const NEVER_FINISHED: &str = "status in ['Idle', 'Running', 'Dropped']";
const NEVER_DROPPED: &str = "status in ['Idle', 'Running', 'Finished']";

/// The path step of the worker's answer `Finish`.
const FINISHED: &str = "Job.Finish -> Finished (answer from worker";

fn verdict(source: &str) -> JointVerdict {
    let automaton = parse_automaton(source).expect("fixture parses");
    let policies = SendAnalysis::new(STARTS).expect("Cedar parses");
    let bound = super::room::test_bound();
    let mut invariants = check_joint(
        &[&automaton],
        Some(&policies),
        bound,
        Duration::from_secs(30),
    )
    .invariants;
    assert_eq!(invariants.len(), 1, "{invariants:?}");
    invariants.remove(0)
}

#[test]
fn a_rule_only_a_module_answer_breaks_fails() {
    // The trigger names no answer, so the kernel takes whatever action the
    // module's result names, Finish among them.
    let verdict = verdict(&job(WASM, None, NEVER_FINISHED));
    let text = verdict.to_string();
    assert_eq!(verdict.tag(), "FAIL", "{text}");
    assert!(text.contains(FINISHED), "{text}");
}

#[test]
fn an_adapters_answer_is_taken_as_a_modules_is() {
    let verdict = verdict(&job(ADAPTER, None, NEVER_FINISHED));
    let text = verdict.to_string();
    assert_eq!(verdict.tag(), "FAIL", "{text}");
    assert!(text.contains(FINISHED), "{text}");
}

#[test]
fn a_call_answers_only_with_the_action_its_trigger_names() {
    let named = verdict(&job(WASM, Some("Finish"), NEVER_FINISHED));
    let text = named.to_string();
    assert_eq!(named.tag(), "FAIL", "{text}");
    assert!(text.contains(FINISHED), "{text}");
    // Drop is no answer of this call, and nobody outside may send it.
    let other = verdict(&job(WASM, Some("Finish"), NEVER_DROPPED));
    assert!(other.holds(), "{other}");
}

#[test]
fn a_call_to_an_adapter_a_field_names_is_proved() {
    // The kernel fills the adapter's name from the record; the proof names
    // the call all the same.
    let adapter = "kind = \"adapter\"\nadapter = \"{field:worker}\"";
    let proved = verdict(&job(adapter, Some("Finish"), NEVER_DROPPED));
    assert_eq!(proved.tag(), "PROVED", "{proved}");
}

#[test]
fn calls_with_colliding_normalized_and_numbered_names_are_proved() {
    for adapters in [
        ["{field:worker}", "_field_worker_", "_field_worker_2"],
        ["_field_worker_2", "{field:worker}", "_field_worker_"],
        ["_field_worker_", "_field_worker_2", "{field:worker}"],
    ] {
        let triggers = adapters
            .map(|adapter| {
                format!(
                    "[[action.triggers]]\nname = \"{adapter}\"\nkind = \"adapter\"\nadapter = \"{adapter}\"\non_success = \"Finish\"\n"
                )
            })
            .join("\n");
        let source = job(ADAPTER, Some("Finish"), NEVER_DROPPED);
        let start = source.find("[[action.triggers]]").unwrap();
        let end = start + source[start..].find("[[action]]").unwrap();
        let source = format!("{}{triggers}{}", &source[..start], &source[end..]);
        let proved = verdict(&source);
        assert_eq!(proved.tag(), "PROVED", "{adapters:?}: {proved}");
    }
}
