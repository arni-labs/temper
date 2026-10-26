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

/// Exercise both the walk alone and the install path, which tries induction
/// first. A proof that masks the walk's counterexample must fail this check.
fn broken_in_both(sources: &[&str], cedar: &str, steps: &[&str]) {
    let automatons: Vec<_> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<_> = automatons.iter().collect();
    let policies = SendAnalysis::new(cedar).expect("Cedar parses");
    let walked = verify_joint_invariants(&refs, Some(&policies), super::room::test_bound());
    assert_eq!(walked.len(), 1);
    assert!(
        matches!(walked[0].outcome, JointOutcome::Broken { .. }),
        "{}",
        walked[0]
    );
    let checked = check_joint(
        &refs,
        Some(&policies),
        super::room::test_bound(),
        Duration::from_secs(30),
    );
    assert_eq!(checked.invariants.len(), 1);
    assert_eq!(
        checked.invariants[0].tag(),
        "FAIL",
        "{}",
        checked.invariants[0]
    );
    for text in [walked[0].to_string(), checked.invariants[0].to_string()] {
        for step in steps {
            assert!(text.contains(step), "missing {step}: {text}");
        }
    }
}

#[test]
fn an_answer_can_match_a_field_changed_after_the_call_started() {
    for kind in [WASM, ADAPTER] {
        for named in ["", "on_success = 'Finish'"] {
            let source = format!(
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
{kind}
{named}

[[action]]
name = "Update"
kind = "input"
from = ["Running"]
to = "Updated"
params = [{{ name = "commit", type = "string" }}]

[[action]]
name = "Finish"
kind = "input"
from = ["Updated"]
to = "Finished"
params = [{{ name = "commit", type = "string" }}]
guard = "commit == 'new'"

[[action.constraints]]
kind = "param_equals_field"
param = "commit"
field = "commit"

[[invariant]]
name = "Rule"
assert = "status in ['Idle', 'Running', 'Updated'] || commit != 'new'"
"#
            );
            let policies = r#"permit(principal, action in [Action::"Start", Action::"Update"], resource is Job);"#;
            broken_in_both(
                &[&source],
                policies,
                &["Job.Start", "Job.Update", "Job.Finish(commit='new')"],
            );
        }
    }
}

#[test]
fn an_adapter_answers_as_platform_dispatch_and_its_reaction_inherits_it() {
    // Finish bypasses Cedar, but its reaction does not. Only the actual
    // background sender may send Poison; nobody outside may send it.
    let source = job(ADAPTER, Some("Finish"), NEVER_FINISHED);
    let source = source.split("[[invariant]]").next().unwrap().replace(
        "[[action]]\nname = \"Drop\"",
        r#"[[action.triggers]]
name = "poison"
kind = "entity"
target_entity = "Sink"
target_action = "Poison"
[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Drop""#,
    );
    let sink = r#"
[automaton]
name = "Sink"
states = ["Idle", "Poisoned"]
initial = "Idle"

[[action]]
name = "Poison"
kind = "input"
from = ["Idle"]
to = "Poisoned"

[[invariant]]
name = "Rule"
assert = "Job[Id].status == 'Finished' => status == 'Idle'"
"#;
    let policies = format!(
        r#"{STARTS}
permit(principal == Agent::"service:platform-dispatch", action == Action::"Poison", resource is Sink);"#
    );
    broken_in_both(
        &[&source, sink],
        &policies,
        &[
            "answer from worker, as service:platform-dispatch",
            "Sink.Poison",
        ],
    );
    let wasm_source = source.replace(ADAPTER, WASM);
    let wasm = parse_automaton(&wasm_source).expect("fixture parses");
    let sink = parse_automaton(sink).expect("fixture parses");
    let policies = SendAnalysis::new(&policies).expect("Cedar parses");
    let denied = check_joint(
        &[&wasm, &sink],
        Some(&policies),
        super::room::test_bound(),
        Duration::from_secs(30),
    );
    assert!(denied.invariants[0].holds(), "{}", denied.invariants[0]);
}
