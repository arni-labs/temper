//! A reaction from a type outside a rule's scope: the walk does not model
//! its source, so the reaction may come at any time, as a move from outside
//! sent as the reaction's sender, and Cedar decides it as that sender.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bytes, Progress};

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

/// `Pre` reacts to `Go` by sending `Push.Poke` as `service:pusher`, so a
/// reaction of Push's without a principal runs as pusher too.
const PRE: &str = r#"
[automaton]
name = "Pre"
states = ["Ready", "Gone"]
initial = "Ready"

[[action]]
name = "Go"
kind = "input"
from = ["Ready"]
to = "Gone"

[[action.triggers]]
name = "poke"
kind = "entity"
principal = "pusher"
target_entity = "Push"
target_action = "Poke"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// Like `pushed`, except that `Start` also asks a module whose trigger names
/// no `on_success`, so the module's result may name `Done`, which the kernel
/// then takes without Cedar.
fn asks_a_module(rule: &str) -> String {
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

[[action]]
name = "Done"
kind = "input"
from = ["Asked"]
to = "Done"
{rule}
"#
    )
}

/// An invariant over Ask and Box that `Ask.Done` breaks, with no premise.
const DONE_FILLS: &str =
    "[[invariant]]\nname = \"rule\"\nassert = \"status != 'Done' || Box[Id].status == 'Full'\"\n";

fn invariant(sources: &[&str], done: &str) -> JointInvariantResult {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(&policies(done)).expect("fixture policies parse");
    let mut results = bounded(|bound| verify_joint_invariants(&refs, Some(&analysis), bound));
    assert_eq!(results.len(), 1);
    results.remove(0)
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

/// Run `check` in a room no fixture here comes near filling.
fn bounded<T>(check: impl FnOnce(Bound<'_>) -> T) -> T {
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    check(Bound {
        room: &room,
        progress: &progress,
    })
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
    let mut results = bounded(|bound| verify_joint_liveness(&refs, Some(&analysis), bound));
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
    let mut results = bounded(|bound| verify_joint_invariants(&refs, Some(&analysis), bound));
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

#[test]
fn a_reaction_from_outside_without_a_principal_may_run_as_a_service_declared_further_out() {
    // Pre pokes Push as pusher, and Push's reaction, with no principal of
    // its own, sends Ask.Done as pusher too. Only pusher may send Done.
    let result = invariant(&[&pushed(DONE_FILLS), BOX, &push(None), PRE], PUSHER_MAY);
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    assert!(
        result
            .to_string()
            .contains("a reaction from Push, as service:pusher"),
        "{result}"
    );
}

#[test]
fn a_module_answer_a_refused_reaction_from_outside_also_sends_is_not_checked() {
    // Push's reaction into Done runs as pusher, whom Cedar refuses, so the
    // walk never takes Done that way. The module may still answer Done, as
    // the runtime and without Cedar, so the rule is not checked.
    let result = invariant(
        &[&asks_a_module(DONE_FILLS), BOX, &push(Some("pusher"))],
        "",
    );
    let text = result.to_string();
    assert!(!result.holds(), "{text}");
    assert!(text.contains("NOT CHECKED"), "{text}");
    assert!(text.contains("Ask.Done (from Asked)"), "{text}");
}
