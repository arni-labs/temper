//! A reaction from a type outside a rule's scope: the walk does not model
//! its source, so the reaction may come at any time, as a move from outside
//! sent as the reaction's sender, and Cedar decides it as that sender.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::*;

/// A record whose `Done` only a reaction from `Push`, a type the rule does
/// not name, sends. `Gone` is a status nothing leads to. `rule` is appended
/// as written.
fn pushed(rule: &str) -> String {
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

[[action]]
name = "Done"
kind = "input"
from = ["Asked"]
to = "Done"
{rule}
"#
    )
}

/// `Push` reacts to `Poke` by sending `Ask.Done`, as `principal` when it
/// declares one.
fn push(principal: Option<&str>) -> String {
    let principal = principal.map_or(String::new(), |name| format!("principal = \"{name}\"\n"));
    format!(
        r#"
[automaton]
name = "Push"
states = ["Ready", "Pushed"]
initial = "Ready"

[[action]]
name = "Poke"
kind = "input"
from = ["Ready"]
to = "Pushed"

[[action.triggers]]
name = "done"
kind = "entity"
{principal}target_entity = "Ask"
target_action = "Done"

[action.triggers.resolve_target]
kind = "same_id"
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

/// Someone outside may start and poke; `done` says who may send `Done`.
fn policies(done: &str) -> String {
    format!(
        r#"permit(principal, action in [Action::"Start", Action::"Poke"], resource);
{done}"#
    )
}

const PUSHER_MAY: &str =
    r#"permit(principal == Agent::"service:pusher", action == Action::"Done", resource);"#;

fn bound() -> Bound<'static> {
    super::room::test_bound()
}

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

fn liveness(principal: Option<&str>, done: &str) -> JointLivenessResult {
    let rule =
        "[[liveness]]\nname = \"rule\"\neventually = \"status == 'Done' => status == 'Gone'\"\n";
    let automatons = parsed(&[&pushed(rule), &push(principal)]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(&policies(done)).expect("fixture policies parse");
    let mut results = verify_joint_liveness(&refs, Some(&analysis), bound());
    assert_eq!(results.len(), 1);
    results.remove(0)
}

#[test]
fn a_reaction_from_outside_the_scope_is_walked() {
    // Push's reaction takes Done, and nothing then leads to Gone. The rule
    // names only Ask, so the walk sends the reaction itself.
    let result = liveness(Some("pusher"), PUSHER_MAY);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
    assert!(
        text.contains("Ask.Done -> Done (a reaction from Push, as service:pusher)"),
        "{text}"
    );
}

#[test]
fn cedar_decides_a_reaction_from_outside_as_its_principal() {
    // Cedar lets pusher send nothing, so the reaction is refused and Done is
    // never taken: the premise holds nowhere.
    let result = liveness(Some("pusher"), "");
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Vacuous { .. }),
        "{text}"
    );
}

#[test]
fn a_reaction_from_outside_without_a_principal_runs_as_any_sender() {
    // Without a principal it runs as whoever sent Poke, which the walk does
    // not know: the runtime answering a module call is one of them.
    let runtime_may = r#"permit(principal == Agent::"service:wasm-runtime", action == Action::"Done", resource);"#;
    let result = liveness(None, runtime_may);
    let text = result.to_string();
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{text}"
    );
    assert!(text.contains("a reaction from Push"), "{text}");
}

#[test]
fn an_invariant_a_reaction_from_outside_breaks_is_broken() {
    let rule = "[[invariant]]\nname = \"rule\"\nassert = \"status == 'Done' => Box[Id].status == 'Full'\"\n";
    let automatons = parsed(&[&pushed(rule), BOX, &push(Some("pusher"))]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(&policies(PUSHER_MAY)).expect("fixture policies parse");
    let mut results = verify_joint_invariants(&refs, Some(&analysis), bound());
    assert_eq!(results.len(), 1);
    let result = results.remove(0);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    assert!(
        result.to_string().contains("a reaction from Push"),
        "{result}"
    );
}
