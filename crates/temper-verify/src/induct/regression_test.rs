//! Regression cases for inherited senders and distinct dispatch identities.

use super::*;
use temper_spec::automaton::parse_automaton;

const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Closed"]
initial = "Closed"

[[state]]
name = "locked"
type = "bool"
initial = true

[[action]]
name = "Init"
kind = "input"
from = ["Closed"]
to = "Closed"
effect = ["schedule('Unlock', 60)"]

[[action]]
name = "Unlock"
kind = "input"
from = ["Closed"]
to = "Closed"
effect = ["locked = false"]

[[invariant]]
name = "locked"
assert = "locked == true"
"#;

const MAKER: &str = r#"
[automaton]
name = "Maker"
states = ["Idle"]
initial = "Idle"

[[action]]
name = "Init"
kind = "input"
from = ["Idle"]
to = "Idle"
effect = ["spawn('Parent', 'Init')"]
"#;

const SOURCE: &str = r#"
[automaton]
name = "Source"
states = ["Idle"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Idle"

[[action.triggers]]
name = "make"
kind = "entity"
principal = "pusher"
target_entity = "Maker"
target_action = "Init"

[action.triggers.resolve_target]
kind = "same_id"
"#;

fn inherited_scheduled_fire(parent: &str, maker: &str, source: &str, allowed: bool, guard: &str) {
    let automatons: Vec<_> = [parent, maker, source]
        .map(|text| parse_automaton(text).expect("fixture parses"))
        .into_iter()
        .collect();
    let refs: Vec<_> = automatons.iter().collect();
    let unlock = if allowed { "pusher" } else { "another" };
    let policies = SendAnalysis::new(&format!(r#"
permit(principal, action == Action::"Start", resource);
permit(principal == Agent::"service:pusher", action == Action::"Init", resource);
permit(principal == Agent::"service:{unlock}", action == Action::"Unlock", resource) when {{ {guard} }};
"#)).expect("fixture policies parse");
    let mut proofs = prove(&refs, Some(&policies), &Hints::default());
    assert_eq!(proofs.len(), 1);
    let proof = proofs.remove(0);
    assert!(proof.errors.is_empty(), "{proof:?}");
    if allowed {
        assert!(
            proof
                .obligations
                .iter()
                .any(|o| o.name == "Parent.Unlock.scheduled fires"
                    && matches!(o.outcome, Outcome::Fails { .. })),
            "{proof:?}"
        );
        assert!(!proof.proved(), "{proof:?}");
    } else {
        assert!(proof.proved(), "{proof:?}");
        assert!(
            !proof
                .obligations
                .iter()
                .any(|o| o.name == "Parent.Unlock.scheduled fires"),
            "{proof:?}"
        );
    }
}

#[test]
fn a_spawn_preserves_the_service_that_arms_a_scheduled_fire() {
    // Both ends call their initializer Init; visited actions must include
    // their entity type. Only the declared service may initialize or unlock.
    inherited_scheduled_fire(PARENT, MAKER, SOURCE, true, "true");
    inherited_scheduled_fire(PARENT, MAKER, SOURCE, false, "true");
}

#[test]
fn a_spawn_does_not_equate_the_inherited_source_id_with_the_child_id() {
    inherited_scheduled_fire(
        PARENT,
        MAKER,
        SOURCE,
        true,
        "context.triggerSourceEntityId != resource.id",
    );
}

#[test]
fn inherited_senders_follow_cycles_and_undeclared_reactions() {
    // Parent -> Maker -> Parent is a spawn cycle, with the service entering
    // through an undeclared reaction Relay -> Maker.
    let parent = PARENT.replace(
        "schedule('Unlock', 60)",
        "schedule('Unlock', 60)\", \"spawn('Maker', 'Init')",
    );
    let relay = MAKER.replace("Maker", "Relay").replace(
        "effect = [\"spawn('Parent', 'Init')\"]",
        r#"
[[action.triggers]]
name = "relay"
kind = "entity"
target_entity = "Maker"
target_action = "Init"

[action.triggers.resolve_target]
kind = "same_id"
"#,
    );
    let source = SOURCE.replace("target_entity = \"Maker\"", "target_entity = \"Relay\"");
    // Keep four automata in this variant, including the relay.
    let automatons: Vec<_> = [parent.as_str(), MAKER, &relay, &source]
        .iter()
        .map(|text| parse_automaton(text).expect("fixture parses"))
        .collect();
    let refs: Vec<_> = automatons.iter().collect();
    let policies = SendAnalysis::new(r#"
permit(principal, action == Action::"Start", resource);
permit(principal == Agent::"service:pusher", action in [Action::"Init", Action::"Unlock"], resource);
"#).expect("fixture policies parse");
    let proof = prove(&refs, Some(&policies), &Hints::default()).remove(0);
    assert!(proof.errors.is_empty(), "{proof:?}");
    assert!(
        proof
            .obligations
            .iter()
            .any(|o| o.name == "Parent.Unlock.scheduled fires"
                && matches!(o.outcome, Outcome::Fails { .. })),
        "{proof:?}"
    );
}

#[test]
fn scheduled_and_reaction_names_have_distinct_solver_declarations() {
    for actions in [
        ["Arm", "Close", "Close.scheduled", "Close.scheduled2"],
        ["Close.scheduled2", "Close.scheduled", "Close", "Arm"],
    ] {
        let mut source = String::from(
            r#"
[automaton]
name = "Parent"
states = ["Closed"]
initial = "Closed"

[[invariant]]
name = "closed"
assert = "status == 'Closed'"
"#,
        );
        for name in actions {
            source.push_str(&format!(
                r#"
[[action]]
name = "{name}"
kind = "input"
from = ["Closed"]
to = "Closed"
"#
            ));
            if name == "Arm" {
                source.push_str("effect = [\"schedule('Close', 60)\"]\n");
            } else {
                source.push_str(&format!(
                    r#"
[[action.triggers]]
name = "send"
kind = "entity"
target_entity = "Parent"
target_action = "{name}"

[action.triggers.resolve_target]
kind = "same_id"
"#
                ));
            }
        }
        let automaton = parse_automaton(&source).expect("fixture parses");
        let answered = Mutex::new(BTreeMap::new());
        let v = Vocab::new(
            BTreeMap::from([("Parent", &automaton)]),
            BTreeSet::from(["Parent".to_string()]),
            None,
            &answered,
        );
        assert_eq!(v.kinds.len(), 4);
        // Replay the actual generated declarations, not just their labels.
        let mut z3 = solver::Z3::start().expect("solver starts");
        z3.send(&prelude(&v, &BTreeSet::new()))
            .expect("prelude runs");
        assert!(z3.errors.is_empty(), "{:?}", z3.errors);
        let proof = prove(&[&automaton], None, &Hints::default()).remove(0);
        assert!(proof.proved(), "{proof:?}");
    }
}
