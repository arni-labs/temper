use super::*;
use temper_spec::automaton::parse_automaton;

#[test]
fn inherited_source_id_does_not_become_a_different_targets_id() {
    // A[id1] -> B[id1] declares the service. B then forwards the
    // principal to C[id2] through a field, retaining A's id1 at runtime.
    let a = parse_automaton(
        r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "first"
kind = "entity"
target_entity = "B"
target_action = "Forward"
principal = "chain"
[action.triggers.resolve_target]
kind = "same_id"
"#,
    )
    .unwrap();
    let policy = SendAnalysis::new(
        r#"
permit(principal == Agent::"service:chain", action, resource) when {
context has triggerSourceEntityId && resource has id &&
context.triggerSourceEntityId == resource.id
};
"#,
    )
    .unwrap();
    let entities = vec!["A".to_string()];
    let senders = Senders::new(&BTreeMap::from([("A", &a)]), &entities, Some(&policy));
    let principal = declared(&a.actions[0].triggers[0], "A", "Start").unwrap();
    let b = senders.reaction(Some(&principal), OUTSIDE, true);
    assert_eq!(senders.may(b, "A", "Start"), May::Yes);
    let c = senders.reaction(None, b, false);
    assert_eq!(senders.may(c, "A", "Start"), May::Maybe);
    // More same-ID hops cannot restore knowledge of the original ID.
    assert_eq!(senders.reaction(None, c, true), c);
    assert_eq!(senders.reaction(None, c, false), c);
    // Same-ID chains preserve knowledge; a declaration starts fresh.
    assert_eq!(senders.reaction(None, b, true), b);
    assert_eq!(senders.reaction(Some(&principal), c, true), b);
}

/// A reaction with `as` is sent by the kernel acting for some user, not
/// by its source's sender: a policy that admits only a user's agents
/// leaves it to the request, and one that admits only services refuses it.
#[test]
fn a_reaction_with_as_is_sent_for_a_user_the_model_does_not_know() {
    let a = parse_automaton(
        r#"
[automaton]
name = "A"
states = ["Ready", "Done"]
initial = "Ready"
strict_action_params = true
[[state]]
name = "person"
type = "string"
initial = ""
[[action]]
name = "Claim"
kind = "input"
from = ["Ready"]
params = [{ name = "person", type = "string", source = "authenticated_subject" }]
[[action]]
name = "Start"
kind = "input"
from = ["Ready"]
to = "Done"
[[action.triggers]]
name = "first"
kind = "entity"
target_entity = "A"
target_action = "Start"
as = "person"
[action.triggers.resolve_target]
kind = "same_id"
"#,
    )
    .unwrap();
    let asker = declared(&a.actions[1].triggers[0], "A", "Start").unwrap();
    assert_eq!(
        asker,
        Asker::Service {
            name: "reaction".to_string(),
            context: BTreeMap::new(),
            acting_for: true,
        }
    );
    let for_ann = SendAnalysis::new(
            r#"permit(principal, action, resource) when { context has actingFor && context.actingFor == "ann" };"#,
        )
        .unwrap();
    let services = SendAnalysis::new(
            r#"permit(principal, action, resource) when { principal has role && principal.role == "service" };"#,
        )
        .unwrap();
    let entities = vec!["A".to_string()];
    let by_name = BTreeMap::from([("A", &a)]);
    let senders = Senders::new(&by_name, &entities, Some(&for_ann));
    let sent = senders.reaction(Some(&asker), OUTSIDE, true);
    assert_eq!(senders.may(sent, "A", "Start"), May::Maybe);
    let senders = Senders::new(&by_name, &entities, Some(&services));
    let sent = senders.reaction(Some(&asker), OUTSIDE, true);
    assert_eq!(senders.may(sent, "A", "Start"), May::No);
}
