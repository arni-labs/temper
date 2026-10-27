//! A pass that checked nothing is not a pass: a rule whose premise no walked
//! state meets proves nothing. A premise only a module's answer reaches is
//! met, since the walk takes every answer the kernel takes.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A record that asks a module when it starts. Its answer `Done` is declared
/// `done_kind`; with `on_success` the trigger names it, and without one the
/// module's result names the answer, as the kernel takes it. `Gone` is a
/// status nothing leads to. `rule` is appended as written.
fn ask(done_kind: &str, on_success: bool, rule: &str) -> String {
    let named = if on_success {
        "on_success = \"Done\"\n"
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Ask"
states = ["Idle", "Asked", "Done", "Gone"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Asked"

[[action.triggers]]
name = "ask"
kind = "wasm"
module = "asker"
{named}
[[action]]
name = "Done"
kind = "{done_kind}"
from = ["Asked"]
to = "Done"
{rule}
"#
    )
}

/// A box nothing fills.
const BOX: &str = r#"
[automaton]
name = "Box"
states = ["Empty", "Full"]
initial = "Empty"

[[action]]
name = "Fill"
kind = "internal"
from = ["Empty"]
to = "Full"
"#;

/// Only `Start` may be sent from outside; a module's answer skips Cedar.
const POLICIES: &str = r#"permit(principal, action == Action::"Start", resource);"#;

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn bound() -> Bound<'static> {
    super::room::test_bound()
}

fn liveness(done_kind: &str, on_success: bool, eventually: &str) -> JointLivenessResult {
    let rule = format!("[[liveness]]\nname = \"rule\"\neventually = \"{eventually}\"\n");
    let automatons = parsed(&[&ask(done_kind, on_success, &rule)]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let mut results = verify_joint_liveness(&refs, Some(&analysis), bound());
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn invariant(done_kind: &str, assert: &str) -> JointInvariantResult {
    let rule = format!("[[invariant]]\nname = \"rule\"\nassert = \"{assert}\"\n");
    let automatons = parsed(&[&ask(done_kind, false, &rule), BOX]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound());
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_liveness_rule_a_module_answer_strands_never_settles() {
    // The module may answer Done, and nothing then leads to Gone: the
    // runtime gets stuck. The walk takes Done as the kernel does.
    let result = liveness("input", false, "status == 'Done' => status == 'Gone'");
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
}

#[test]
fn a_liveness_premise_no_walked_state_meets_proves_nothing() {
    let result = liveness("internal", false, "status == 'Gone' => status == 'Done'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("CHECKS NOTHING AS WRITTEN"), "{text}");
    assert!(text.contains("`status == 'Gone'`"), "{text}");
}

#[test]
fn an_invariant_a_module_answer_breaks_is_broken() {
    // Once the module answers Done, the box is still Empty: the runtime
    // breaks the rule, and so does the walk.
    let result = invariant("input", "status == 'Done' => Box[Id].status == 'Full'");
    let text = result.to_string();
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{text}"
    );
    assert!(
        text.contains("Ask.Done -> Done (answer from asker"),
        "{text}"
    );
}

#[test]
fn an_invariant_premise_no_settled_state_meets_proves_nothing() {
    let result = invariant("internal", "status == 'Gone' => Box[Id].status == 'Full'");
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("CHECKS NOTHING AS WRITTEN"), "{text}");
}

#[test]
fn a_premise_the_walk_meets_settles() {
    let result = liveness("internal", false, "status == 'Done' => status == 'Done'");
    assert!(result.holds(), "{result}");
}

#[test]
fn an_answer_the_trigger_names_is_walked() {
    // Done is declared input and no one outside may send it, but the
    // trigger names it, so the model takes it as the module's answer.
    let result = liveness("input", true, "status == 'Done' => status == 'Done'");
    assert!(result.holds(), "{result}");
}

#[test]
fn an_invariant_the_walk_breaks_stays_broken() {
    let result = invariant("internal", "status == 'Done' => Box[Id].status == 'Full'");
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
}
